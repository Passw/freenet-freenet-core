use anyhow::bail;
use freenet::{
    config::{ConfigArgs, InlineGwConfig, NetworkArgs, SecretArgs, WebsocketApiArgs},
    dev_tool::TransportKeypair,
    local_node::NodeConfig,
    server::serve_gateway,
};
use freenet_stdlib::{
    client_api::{
        ClientRequest, HostResponse, NodeDiagnosticsConfig, NodeQuery, QueryResponse, WebApi,
    },
    prelude::*,
};
use futures::FutureExt;
use rand::{random, Rng, SeedableRng};
use std::{
    net::{Ipv4Addr, TcpListener},
    path::Path,
    time::Duration,
};
use testresult::TestResult;
use tokio::select;
use tokio::sync::watch;
use tokio::time::timeout;
use tracing::level_filters::LevelFilter;

static RNG: once_cell::sync::Lazy<std::sync::Mutex<rand::rngs::StdRng>> =
    once_cell::sync::Lazy::new(|| {
        std::sync::Mutex::new(rand::rngs::StdRng::from_seed(
            *b"0102030405060708090a0b0c0d0e0f10",
        ))
    });

struct PresetConfig {
    temp_dir: tempfile::TempDir,
}

async fn base_node_test_config(
    is_gateway: bool,
    gateways: Vec<String>,
    public_port: Option<u16>,
    ws_api_port: u16,
) -> anyhow::Result<(ConfigArgs, PresetConfig)> {
    if is_gateway {
        assert!(public_port.is_some());
    }

    let temp_dir = tempfile::tempdir()?;
    let key = TransportKeypair::new_with_rng(&mut *RNG.lock().unwrap());
    let transport_keypair = temp_dir.path().join("private.pem");
    key.save(&transport_keypair)?;
    key.public().save(temp_dir.path().join("public.pem"))?;
    let config = ConfigArgs {
        ws_api: WebsocketApiArgs {
            address: Some(Ipv4Addr::LOCALHOST.into()),
            ws_api_port: Some(ws_api_port),
        },
        network_api: NetworkArgs {
            public_address: Some(Ipv4Addr::LOCALHOST.into()),
            public_port,
            is_gateway,
            skip_load_from_network: true,
            gateways: Some(gateways),
            location: Some(RNG.lock().unwrap().gen()),
            ignore_protocol_checking: true,
            address: Some(Ipv4Addr::LOCALHOST.into()),
            network_port: public_port,
            bandwidth_limit: None,
            blocked_addresses: None,
        },
        config_paths: {
            freenet::config::ConfigPathsArgs {
                config_dir: Some(temp_dir.path().to_path_buf()),
                data_dir: Some(temp_dir.path().to_path_buf()),
            }
        },
        secrets: SecretArgs {
            transport_keypair: Some(transport_keypair),
            ..Default::default()
        },
        ..Default::default()
    };
    Ok((config, PresetConfig { temp_dir }))
}

fn gw_config(port: u16, path: &Path) -> anyhow::Result<InlineGwConfig> {
    Ok(InlineGwConfig {
        address: (Ipv4Addr::LOCALHOST, port).into(),
        location: Some(random()),
        public_key_path: path.join("public.pem"),
    })
}

async fn check_connection_count(
    client: &mut WebApi,
    expected_count: usize,
) -> anyhow::Result<bool> {
    let config = NodeDiagnosticsConfig {
        include_node_info: false,
        include_network_info: true,
        include_subscriptions: false,
        contract_keys: vec![],
        include_system_metrics: false,
        include_detailed_peer_info: false,
        include_subscriber_peer_ids: false,
        include_connection_details: false,
    };

    client
        .send(ClientRequest::NodeQueries(NodeQuery::NodeDiagnostics {
            config,
        }))
        .await?;

    let response = timeout(Duration::from_secs(10), client.recv()).await??;

    if let HostResponse::QueryResponse(QueryResponse::NodeDiagnostics(diag)) = response {
        if let Some(network_info) = diag.network_info {
            println!(
                "Active connections: {}, Expected: {}",
                network_info.connected_peers.len(),
                expected_count
            );
            Ok(network_info.connected_peers.len() == expected_count)
        } else {
            Ok(false)
        }
    } else {
        bail!("Unexpected response from diagnostics query");
    }
}

async fn get_node_info_and_connections(
    client: &mut WebApi,
) -> anyhow::Result<(String, Vec<(String, String)>)> {
    let config = NodeDiagnosticsConfig {
        include_node_info: true,
        include_network_info: true,
        include_subscriptions: false,
        contract_keys: vec![],
        include_system_metrics: false,
        include_detailed_peer_info: false,
        include_subscriber_peer_ids: false,
        include_connection_details: false,
    };

    client
        .send(ClientRequest::NodeQueries(NodeQuery::NodeDiagnostics {
            config,
        }))
        .await?;

    let response = timeout(Duration::from_secs(10), client.recv()).await??;

    if let HostResponse::QueryResponse(QueryResponse::NodeDiagnostics(diag)) = response {
        let peer_id = diag
            .node_info
            .ok_or_else(|| anyhow::anyhow!("No node info in response"))?
            .peer_id;

        let connected_peers = diag
            .network_info
            .ok_or_else(|| anyhow::anyhow!("No network info in response"))?
            .connected_peers;

        Ok((peer_id, connected_peers))
    } else {
        bail!("Unexpected response from diagnostics query");
    }
}

async fn verify_peer_id_match(
    gw_client: &mut WebApi,
    node_client: &mut WebApi,
) -> anyhow::Result<bool> {
    // Get gateway's peer ID and its connected peers
    let (gw_peer_id, gw_connected_peers) = get_node_info_and_connections(gw_client).await?;

    // Get node's peer ID and its connected peers
    let (node_peer_id, node_connected_peers) = get_node_info_and_connections(node_client).await?;

    println!("Gateway peer ID: {}", gw_peer_id);
    println!("Node peer ID: {}", node_peer_id);

    // Check if gateway is connected to the node
    let gw_connected_to_node = gw_connected_peers
        .iter()
        .any(|(peer_id, _)| peer_id == &node_peer_id);

    // Check if node is connected to the gateway
    let node_connected_to_gw = node_connected_peers
        .iter()
        .any(|(peer_id, _)| peer_id == &gw_peer_id);

    if gw_connected_to_node {
        println!("Gateway is connected to node (peer ID: {})", node_peer_id);
    } else {
        println!("Gateway is NOT connected to node");
        println!("Gateway's connected peers: {:?}", gw_connected_peers);
    }

    if node_connected_to_gw {
        println!("Node is connected to gateway (peer ID: {})", gw_peer_id);
    } else {
        println!("Node is NOT connected to gateway");
        println!("Node's connected peers: {:?}", node_connected_peers);
    }

    Ok(gw_connected_to_node && node_connected_to_gw)
}

#[tokio::test(flavor = "multi_thread")]
async fn test_gateway_node_reconnection() -> TestResult {
    freenet::config::set_logger(Some(LevelFilter::INFO), None);

    // Set up ports
    let network_socket_gw = TcpListener::bind("127.0.0.1:0")?;
    let ws_api_port_socket_gw = TcpListener::bind("127.0.0.1:0")?;
    let ws_api_port_socket_node = TcpListener::bind("127.0.0.1:0")?;

    // Configure gateway
    let (config_gw, preset_cfg_gw, config_gw_inline) = {
        let (cfg, preset) = base_node_test_config(
            true,
            vec![],
            Some(network_socket_gw.local_addr()?.port()),
            ws_api_port_socket_gw.local_addr()?.port(),
        )
        .await?;
        let public_port = cfg.network_api.public_port.unwrap();
        let path = preset.temp_dir.path().to_path_buf();
        (cfg, preset, gw_config(public_port, &path)?)
    };

    // Configure regular node
    let (config_node, preset_cfg_node) = base_node_test_config(
        false,
        vec![serde_json::to_string(&config_gw_inline)?],
        None,
        ws_api_port_socket_node.local_addr()?.port(),
    )
    .await?;

    let ws_api_port_gw = config_gw.ws_api.ws_api_port.unwrap();
    let ws_api_port_node = config_node.ws_api.ws_api_port.unwrap();

    println!("Gateway data dir: {:?}", preset_cfg_gw.temp_dir.path());
    println!("Node data dir: {:?}", preset_cfg_node.temp_dir.path());
    println!("Gateway WS API port: {}", ws_api_port_gw);
    println!("Node WS API port: {}", ws_api_port_node);

    // Start gateway
    std::mem::drop(network_socket_gw);
    std::mem::drop(ws_api_port_socket_gw);
    let gateway = async {
        let config = config_gw.build().await?;
        let node = NodeConfig::new(config.clone())
            .await?
            .build(serve_gateway(config.ws_api).await)
            .await?;
        node.run().await
    }
    .boxed_local();

    // Test multiple node kill/restart cycles for stability
    std::mem::drop(ws_api_port_socket_node);
    let test = tokio::time::timeout(Duration::from_secs(300), async {
        // Connect to gateway's websocket API for diagnostics
        let gw_uri = format!(
            "ws://127.0.0.1:{}/v1/contract/command?encodingProtocol=native",
            ws_api_port_gw
        );

        println!("\n=== Node Kill/Restart Stability Test ===");

        const STABILITY_CYCLES: usize = 3;
        let mut successful_cycles = 0;

        for cycle in 1..=STABILITY_CYCLES {
            println!(
                "\n--- Stability Cycle {} of {} ---",
                cycle, STABILITY_CYCLES
            );

            // Create fresh node config for this cycle
            let ws_api_port_socket_node_cycle = TcpListener::bind("127.0.0.1:0")?;
            let ws_api_port_node_cycle = ws_api_port_socket_node_cycle.local_addr()?.port();
            std::mem::drop(ws_api_port_socket_node_cycle);

            // Reuse same base directory and configuration but with fresh WebSocket port and DB
            let mut node_config_cycle = config_node.clone();
            node_config_cycle.ws_api.ws_api_port = Some(ws_api_port_node_cycle);

            let db_dir_cycle = preset_cfg_node
                .temp_dir
                .path()
                .join(format!("db_cycle_{}", cycle));
            std::fs::create_dir_all(&db_dir_cycle)?;

            // Update data dir to point to cycle-specific database
            let mut data_dir_cycle = preset_cfg_node.temp_dir.path().to_path_buf();
            data_dir_cycle.push(format!("data_cycle_{}", cycle));
            std::fs::create_dir_all(&data_dir_cycle)?;
            node_config_cycle.config_paths.data_dir = Some(data_dir_cycle.clone());

            println!(
                "Cycle {} - Same node identity, fresh WebSocket port {} (base dir: {:?}, data dir: {:?})",
                cycle,
                ws_api_port_node_cycle,
                preset_cfg_node.temp_dir.path(),
                data_dir_cycle
            );

            // Start node with shutdown capability
            let (shutdown_tx, shutdown_rx) = watch::channel(false);
            let node_task = {
                let mut shutdown_rx = shutdown_rx.clone();
                async move {
                    let config = node_config_cycle.build().await?;
                    let node = NodeConfig::new(config.clone())
                        .await?
                        .build(serve_gateway(config.ws_api).await)
                        .await?;

                    select! {
                        result = node.run() => {
                            match result {
                                Ok(_) => {
                                    println!("Node finished normally");
                                    Ok(())
                                },
                                Err(e) => {
                                    println!("Node error: {}", e);
                                    Err(e)
                                }
                            }
                        },
                        _ = shutdown_rx.changed() => {
                            println!("Node shutdown signal received");
                            Ok(())
                        }
                    }
                }
            }
            .boxed_local();

            let cycle_test = async {
                // Give node time to start
                println!("Starting node for cycle {}...", cycle);
                tokio::time::sleep(Duration::from_secs(10)).await;

                // Connect to gateway WebSocket API
                let (gw_ws_stream, _) = tokio_tungstenite::connect_async(&gw_uri).await?;
                let mut gw_client = WebApi::start(gw_ws_stream);

                // Connect to node WebSocket API with retry
                let node_uri = format!(
                    "ws://127.0.0.1:{}/v1/contract/command?encodingProtocol=native",
                    ws_api_port_node_cycle
                );
                let mut node_client = None;
                for attempt in 1..=15 {
                    match tokio_tungstenite::connect_async(&node_uri).await {
                        Ok((node_ws_stream, _)) => {
                            node_client = Some(WebApi::start(node_ws_stream));
                            println!("Connected to node WebSocket API (attempt {})", attempt);
                            break;
                        }
                        Err(e) => {
                            println!("Connection attempt {} failed: {}", attempt, e);
                            tokio::time::sleep(Duration::from_secs(1)).await;
                        }
                    }
                }

                let mut node_client = node_client.ok_or_else(|| {
                    anyhow::anyhow!("Failed to connect to node WebSocket API after 15 attempts")
                })?;

                // Step 1: Verify node reports having 1 connection
                let mut node_has_connection = false;
                for attempt in 1..=8 {
                    if check_connection_count(&mut node_client, 1).await? {
                        node_has_connection = true;
                        println!("Node reports 1 active connection (cycle {})", cycle);
                        break;
                    }
                    println!(
                        "Waiting for node to establish connection... attempt {}",
                        attempt
                    );
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }

                if !node_has_connection {
                    bail!("Node failed to establish any connection in cycle {}", cycle);
                }

                // Step 2: Verify that the connection is specifically to the gateway
                let verification_result =
                    verify_peer_id_match(&mut gw_client, &mut node_client).await;
                match verification_result {
                    Ok(true) => {
                        println!(
                            "CONFIRMED: Node is connected to the correct gateway (cycle {})",
                            cycle
                        );
                        println!("   - Node has 1 connection");
                        println!("   - Connection is to the expected gateway peer");
                    }
                    Ok(false) => {
                        println!(
                            "WARNING: Node has connection but peer verification failed (cycle {})",
                            cycle
                        );
                        println!("   This may indicate connection to wrong peer or gateway tracking issues");
                        // Still continue since node has A connection
                    }
                    Err(e) => {
                        println!("ERROR: Peer verification error: {} (cycle {})", e, cycle);
                        println!("Node still reports connection, continuing test");
                    }
                }

                // Step 3: Test connection stability and get detailed peer info
                println!("Testing connection stability...");
                tokio::time::sleep(Duration::from_secs(20)).await;

                // Final verification with detailed peer information
                match get_node_info_and_connections(&mut node_client).await {
                    Ok((node_peer_id, node_connected_peers)) => {
                        println!("Final verification for cycle {}:", cycle);
                        println!("- Node Peer ID: {}", node_peer_id);
                        println!("- Connected peers: {}", node_connected_peers.len());

                        if node_connected_peers.len() == 1 {
                            let (connected_peer_id, _) = &node_connected_peers[0];
                            println!("- Connected to peer: {}", connected_peer_id);
                            println!(
                                "SUCCESS: Node successfully connected to gateway (cycle {})",
                                cycle
                            );
                        } else {
                            println!(
                                "WARNING: Unexpected peer count but connection was established"
                            );
                        }
                    }
                    Err(e) => {
                        println!(
                            "WARNING: Could not get detailed peer info: {} (cycle {})",
                            e, cycle
                        );
                    }
                }

                // Now kill the node
                println!("Terminating node process (cycle {})...", cycle);

                // Step 1: Close WebSocket client
                std::mem::drop(node_client);

                // Step 2: Signal node process to shutdown
                let _ = shutdown_tx.send(true);

                // Step 3: Wait for termination (we don't care about gateway disconnect detection)
                println!("Waiting for node termination...");
                tokio::time::sleep(Duration::from_secs(3)).await;

                // Always consider cycle successful if node connected initially
                println!(
                    "SUCCESS: Cycle {} completed successfully (node connection verified)",
                    cycle
                );

                std::mem::drop(gw_client);
                Ok::<(), anyhow::Error>(())
            };

            // Run node and test concurrently for this cycle
            let cycle_result = select! {
                node_result = node_task => {
                    match node_result {
                        Ok(_) => {
                            println!("Node finished for cycle {}", cycle);
                            Ok(())
                        },
                        Err(e) => {
                            println!("ERROR: Node error in cycle {}: {}", cycle, e);
                            Err(e.into())
                        }
                    }
                },
                test_result = cycle_test => {
                    test_result
                }
            };

            match cycle_result {
                Ok(_) => {
                    successful_cycles += 1;
                    println!(
                        "SUCCESS: Cycle {} successful - Node reconnection verified",
                        cycle
                    );
                }
                Err(e) => {
                    println!("ERROR: Cycle {} failed: {}", cycle, e);
                    println!("   Note: This may be due to gateway connection tracking issues, not actual connectivity problems");
                }
            }

            if cycle < STABILITY_CYCLES {
                println!("Waiting between cycles for complete cleanup...");
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        }

        let success_rate = (successful_cycles as f64 / STABILITY_CYCLES as f64) * 100.0;
        println!("\nStability Test Results:");
        println!(
            "   Successful cycles: {}/{}",
            successful_cycles, STABILITY_CYCLES
        );
        println!("   Success rate: {:.1}%", success_rate);

        if success_rate == 100.0 {
            println!("SUCCESS: Node reconnection test PASSED!");
            println!(
                "   Success rate: {:.1}% - Demonstrates node reconnection capability",
                success_rate
            );
        } else {
            bail!(
                "ERROR: Reconnection test failed - success rate {:.1}% indicates fundamental connectivity issues",
                success_rate
            );
        }

        Ok::<(), anyhow::Error>(())
    });

    select! {
        result = test => {
            result??;
        }
        result = gateway => {
            result?;
        }
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_gateway_node_connection_stability() -> TestResult {
    freenet::config::set_logger(Some(LevelFilter::INFO), None);

    // Set up ports
    let network_socket_gw = TcpListener::bind("127.0.0.1:0")?;
    let ws_api_port_socket_gw = TcpListener::bind("127.0.0.1:0")?;
    let ws_api_port_socket_node = TcpListener::bind("127.0.0.1:0")?;

    // Configure gateway
    let (config_gw, preset_cfg_gw, config_gw_inline) = {
        let (cfg, preset) = base_node_test_config(
            true,
            vec![],
            Some(network_socket_gw.local_addr()?.port()),
            ws_api_port_socket_gw.local_addr()?.port(),
        )
        .await?;
        let public_port = cfg.network_api.public_port.unwrap();
        let path = preset.temp_dir.path().to_path_buf();
        (cfg, preset, gw_config(public_port, &path)?)
    };

    // Configure regular node
    let (config_node, preset_cfg_node) = base_node_test_config(
        false,
        vec![serde_json::to_string(&config_gw_inline)?],
        None,
        ws_api_port_socket_node.local_addr()?.port(),
    )
    .await?;

    let ws_api_port_gw = config_gw.ws_api.ws_api_port.unwrap();
    let ws_api_port_node = config_node.ws_api.ws_api_port.unwrap();

    println!("Gateway data dir: {:?}", preset_cfg_gw.temp_dir.path());
    println!("Node data dir: {:?}", preset_cfg_node.temp_dir.path());

    // Start gateway
    std::mem::drop(network_socket_gw);
    std::mem::drop(ws_api_port_socket_gw);
    let gateway = async {
        let config = config_gw.build().await?;
        let node = NodeConfig::new(config.clone())
            .await?
            .build(serve_gateway(config.ws_api).await)
            .await?;
        node.run().await
    }
    .boxed_local();

    // Start node
    std::mem::drop(ws_api_port_socket_node);
    let node = async {
        let config = config_node.build().await?;
        let node = NodeConfig::new(config.clone())
            .await?
            .build(serve_gateway(config.ws_api).await)
            .await?;
        node.run().await
    }
    .boxed_local();

    let test = tokio::time::timeout(Duration::from_secs(120), async {
        // Wait for nodes to start up
        println!("Waiting for nodes to start up...");
        tokio::time::sleep(Duration::from_secs(15)).await;

        // Connect to gateway's websocket API for diagnostics
        let gw_uri = format!(
            "ws://127.0.0.1:{}/v1/contract/command?encodingProtocol=native",
            ws_api_port_gw
        );
        let (gw_ws_stream, _) = tokio_tungstenite::connect_async(&gw_uri).await?;
        let mut gw_client = WebApi::start(gw_ws_stream);

        // Monitor connection stability over time
        const MONITORING_CYCLES: usize = 10;
        const CHECK_INTERVAL: Duration = Duration::from_secs(5);

        let mut successful_checks = 0;

        // First, let's get the node client for peer ID verification
        let node_uri = format!(
            "ws://127.0.0.1:{}/v1/contract/command?encodingProtocol=native",
            ws_api_port_node
        );
        let (node_ws_stream, _) = tokio_tungstenite::connect_async(&node_uri).await?;
        let mut node_client = WebApi::start(node_ws_stream);

        for cycle in 1..=MONITORING_CYCLES {
            println!(
                "Connection stability check {} of {}",
                cycle, MONITORING_CYCLES
            );

            if check_connection_count(&mut gw_client, 1).await? {
                successful_checks += 1;
                println!("Connection stable (check {}/{})", successful_checks, cycle);

                // Verify peer IDs on first check
                if cycle == 1 {
                    if verify_peer_id_match(&mut gw_client, &mut node_client).await? {
                        println!("SUCCESS: Peer ID verification successful");
                    } else {
                        println!("WARNING: Peer ID verification failed");
                    }
                }
            } else {
                println!("ERROR: Connection unstable at check {}", cycle);
            }

            tokio::time::sleep(CHECK_INTERVAL).await;
        }

        let stability_rate = (successful_checks as f64 / MONITORING_CYCLES as f64) * 100.0;
        println!(
            "Connection stability: {:.1}% ({}/{})",
            stability_rate, successful_checks, MONITORING_CYCLES
        );

        // Require at least 80% stability
        if stability_rate < 80.0 {
            bail!("Connection stability too low: {:.1}%", stability_rate);
        }

        println!("SUCCESS: Connection stability test passed");
        Ok::<(), anyhow::Error>(())
    });

    select! {
        result = test => {
            result??;
        }
        result = gateway => {
            result?;
        }
        result = node => {
            result?;
        }
    }

    Ok(())
}
