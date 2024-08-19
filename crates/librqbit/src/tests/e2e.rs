use std::{
    net::{Ipv4Addr, SocketAddr},
    time::Duration,
};

use anyhow::{bail, Context};
use librqbit_core::magnet::Magnet;
use rand::Rng;
use tokio::{
    spawn,
    time::{interval, timeout},
};
use tracing::{error_span, info, Instrument};

use crate::{
    create_torrent,
    tests::test_util::{create_default_random_dir_with_torrents, TestPeerMetadata},
    AddTorrentOptions, AddTorrentResponse, Session, SessionOptions,
};

const TIMEOUT_SECS: u64 = 180;

#[tokio::test(flavor = "multi_thread", worker_threads = 64)]
async fn test_e2e_download() {
    tokio::time::timeout(Duration::from_secs(TIMEOUT_SECS), _test_e2e_download())
        .await
        .unwrap()
}

async fn _test_e2e_download() {
    let _ = tracing_subscriber::fmt::try_init();

    // 1. Create a torrent
    // Ideally (for a more complicated test) with N files, and at least N pieces that span 2 files.

    let piece_length: u32 = 16384 * 2; // TODO: figure out if this should be multiple of chunk size or not
    let file_length: usize = 1000 * 1000;
    let num_files: usize = 64;

    let tempdir =
        create_default_random_dir_with_torrents(num_files, file_length, Some("rqbit_e2e"));
    let torrent_file = create_torrent(
        dbg!(tempdir.path()),
        crate::CreateTorrentOptions {
            piece_length: Some(piece_length),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let num_servers = std::env::var("E2E_NUM_SERVERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(128u8);

    let torrent_file_bytes = torrent_file.as_bytes().unwrap();
    let mut futs = Vec::new();

    // 2. Start N servers that are serving that torrent, and return their IP:port combos.
    //    Disable DHT on each.
    for i in 0..num_servers {
        let torrent_file_bytes = torrent_file_bytes.clone();
        let tempdir = tempdir.path().to_owned();
        let fut = spawn(
            async move {
                let peer_id = TestPeerMetadata {
                    server_id: i,
                    max_random_sleep_ms: rand::thread_rng().gen_range(0u8..16),
                }
                .as_peer_id();
                let listen_range_start = 15100u16 + i as u16;
                let listen_range_end = listen_range_start + 1;
                let listen_range = listen_range_start..listen_range_end;
                let session = crate::Session::new_with_opts(
                    std::env::temp_dir().join("does_not_exist"),
                    SessionOptions {
                        disable_dht: true,
                        disable_dht_persistence: true,
                        dht_config: None,
                        persistence: None,
                        peer_id: Some(peer_id),
                        peer_opts: None,
                        listen_port_range: Some(listen_range),
                        enable_upnp_port_forwarding: false,
                        default_storage_factory: None,
                        defer_writes_up_to: None,
                        root_span: Some(error_span!(parent: None, "server", id = i)),
                        ..Default::default()
                    },
                )
                .await
                .context("error starting session")?;

                info!("started session");

                let handle = session
                    .add_torrent(
                        crate::AddTorrent::TorrentFileBytes(torrent_file_bytes),
                        Some(AddTorrentOptions {
                            overwrite: true,
                            output_folder: Some(tempdir.to_str().unwrap().to_owned()),
                            ..Default::default()
                        }),
                    )
                    .await
                    .context("error adding torrent")?;
                let h = handle.into_handle().context("into_handle()")?;
                let mut interval = interval(Duration::from_millis(100));

                info!("added torrent");
                loop {
                    interval.tick().await;
                    let is_live = h
                        .with_state(|s| match s {
                            crate::ManagedTorrentState::Initializing(_) => Ok(false),
                            crate::ManagedTorrentState::Live(l) => {
                                if !l.is_finished() {
                                    bail!("torrent went live, but expected it to be finished");
                                }
                                Ok(true)
                            }
                            crate::ManagedTorrentState::Error(e) => bail!("error: {e:?}"),
                            _ => bail!("broken state"),
                        })
                        .context("error checking for torrent liveness")?;
                    if is_live {
                        break;
                    }
                }
                info!("torrent is live");
                Ok::<_, anyhow::Error>(SocketAddr::new(
                    std::net::IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
                    session
                        .tcp_listen_port()
                        .context("expected session.tcp_listen_port() to be set")?,
                ))
            }
            .instrument(error_span!("server", id = i)),
        );
        futs.push(timeout(Duration::from_secs(30), fut));
    }

    let mut peers = Vec::new();
    for (id, peer) in futures::future::join_all(futs)
        .await
        .into_iter()
        .enumerate()
    {
        let peer = peer
            .with_context(|| format!("join error, server={id}"))
            .unwrap()
            .with_context(|| format!("timeout, server={id}"))
            .unwrap()
            .with_context(|| format!("server couldn't start, server={id}"))
            .unwrap();
        peers.push(peer);
    }

    info!("started all servers, starting client");

    let client_iters = std::env::var("E2E_CLIENT_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1usize);

    let magnet = Magnet::from_id20(torrent_file.info_hash(), Vec::new()).to_string();

    // 3. Start a client with the initial peers, and download the file.
    for _ in 0..client_iters {
        let outdir = tempfile::TempDir::with_prefix("rqbit_e2e_client").unwrap();
        let session = Session::new_with_opts(
            outdir.path().to_owned(),
            SessionOptions {
                disable_dht: true,
                disable_dht_persistence: true,
                dht_config: None,
                persistence: None,
                listen_port_range: None,
                enable_upnp_port_forwarding: false,
                root_span: Some(error_span!("client")),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        info!("started client session");

        let (id, handle) = {
            let r = session
                .add_torrent(
                    crate::AddTorrent::Url((&magnet).into()),
                    Some(AddTorrentOptions {
                        initial_peers: Some(peers.clone()),
                        // only_files: Some(vec![0]),
                        overwrite: false,
                        ..Default::default()
                    }),
                )
                .await
                .unwrap();

            match r {
                AddTorrentResponse::AlreadyManaged(_, _) => todo!(),
                AddTorrentResponse::ListOnly(_) => todo!(),
                AddTorrentResponse::Added(id, h) => (id, h),
            }
        };

        info!("added handle");

        {
            let stats_printer = {
                let handle = handle.clone();
                async move {
                    let mut interval = interval(Duration::from_millis(100));

                    loop {
                        interval.tick().await;
                        let stats = handle.stats();
                        let live = match &stats.live {
                            Some(live) => live,
                            None => continue,
                        };
                        let pstats = &live.snapshot.peer_stats;

                        info!(
                            progress_percent =
                                format!("{}", stats.progress_percent_human_readable()),
                            peers = format!("{:?}", pstats),
                        );
                    }
                }
            }
            .instrument(error_span!("stats_printer"));

            let timeout = timeout(Duration::from_secs(180), handle.wait_until_completed());

            tokio::pin!(stats_printer);
            tokio::pin!(timeout);

            let mut stats_finished = false;
            loop {
                tokio::select! {
                    r = &mut timeout => {
                        r.unwrap().unwrap();
                        break;
                    }
                    _ = &mut stats_printer, if !stats_finished => {
                        stats_finished = true;
                    }
                }
            }
        }

        info!("handle is completed");
        session.delete(id.into(), false).await.unwrap();

        info!("deleted handle");

        // 4. After downloading, recheck its integrity.
        let handle = session
            .add_torrent(
                crate::AddTorrent::TorrentFileBytes(torrent_file_bytes.clone()),
                Some(AddTorrentOptions {
                    paused: true,
                    overwrite: true,
                    ..Default::default()
                }),
            )
            .await
            .unwrap()
            .into_handle()
            .unwrap();

        info!("re-added handle");

        timeout(Duration::from_secs(30), async {
            let mut interval = interval(Duration::from_millis(100));
            loop {
                interval.tick().await;
                let b = handle
                    .with_state(|s| match s {
                        crate::ManagedTorrentState::Initializing(_) => Ok(false),
                        crate::ManagedTorrentState::Paused(p) => {
                            assert_eq!(p.chunk_tracker.get_hns().needed_bytes, 0);
                            Ok(true)
                        }
                        _ => bail!("bugged state"),
                    })
                    .unwrap();
                if b {
                    break;
                }
            }
        })
        .await
        .unwrap();

        info!("all good");
    }
}
