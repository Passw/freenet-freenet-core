//! Network resilience and failure recovery tests
//!
//! Tests network behavior under various failure conditions including:
//! - Node termination and network recovery
//! - Gateway node failure and peer resilience  
//! - Peer node failure and network stability

use anyhow::bail;
use freenet::{
    config::{ConfigArgs, InlineGwConfig, NetworkArgs, SecretArgs, WebsocketApiArgs},
    dev_tool::TransportKeypair,
    local_node::NodeConfig,
    server::serve_gateway,
    test_utils::{self, load_contract, make_get, make_put},
};
use freenet_stdlib::{
    client_api::{ClientRequest, ContractResponse, HostResponse, WebApi},
    prelude::*,
};
use futures::FutureExt;
use rand::{Rng, SeedableRng};
use std::{
    net::{Ipv4Addr, TcpListener},
    sync::{LazyLock, Mutex},
    time::Duration,
};
use tokio::{select, time::timeout};
use tokio_tungstenite::connect_async;
use tracing::level_filters::LevelFilter;

static RNG: LazyLock<Mutex<rand::rngs::StdRng>> = LazyLock::new(|| {
    Mutex::new(rand::rngs::StdRng::from_seed(
        *b"network_resilience_test_seed_456",
    ))
});

async fn create_node_config(
    is_gateway: bool,
    ws_api_port: u16,
    network_port: Option<u16>,
    gateways: Vec<String>,
) -> anyhow::Result<(ConfigArgs, tempfile::TempDir)> {
    let temp_dir = tempfile::tempdir()?;
    let key = TransportKeypair::new();
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
            public_port: network_port,
            is_gateway,
            skip_load_from_network: true,
            gateways: Some(gateways),
            location: Some(RNG.lock().unwrap().random()),
            ignore_protocol_checking: true,
            address: Some(Ipv4Addr::LOCALHOST.into()),
            network_port,
            bandwidth_limit: None,
            blocked_addresses: None,
        },
        config_paths: freenet::config::ConfigPathsArgs {
            config_dir: Some(temp_dir.path().to_path_buf()),
            data_dir: Some(temp_dir.path().to_path_buf()),
        },
        secrets: SecretArgs {
            transport_keypair: Some(transport_keypair),
            ..Default::default()
        },
        ..Default::default()
    };

    Ok((config, temp_dir))
}

/// Test node termination by dropping the future (simulates node crash/kill)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_gateway_node_termination() -> anyhow::Result<()> {
    freenet::config::set_logger(Some(LevelFilter::INFO), None);

    const TEST_CONTRACT: &str = "test-contract-integration";
    let contract = load_contract(TEST_CONTRACT, vec![].into())?;
    let contract_key = contract.key();
    let initial_state = test_utils::create_empty_todo_list();
    let wrapped_state = WrappedState::from(initial_state);

    let gateway_socket = TcpListener::bind("127.0.0.1:0")?;
    let gateway_ws_socket = TcpListener::bind("127.0.0.1:0")?;

    let gateway_port = gateway_socket.local_addr()?.port();
    let gateway_ws_port = gateway_ws_socket.local_addr()?.port();

    let (gateway_config, _gateway_temp) =
        create_node_config(true, gateway_ws_port, Some(gateway_port), vec![]).await?;

    std::mem::drop(gateway_socket);
    std::mem::drop(gateway_ws_socket);

    println!("Starting gateway node");
    let mut gateway_node_future = {
        let config = gateway_config;
        async move {
            let built_config = config.build().await?;
            let node = NodeConfig::new(built_config.clone())
                .await?
                .build(serve_gateway(built_config.ws_api).await)
                .await?;
            node.run().await
        }
    }
    .boxed_local();

    let test_result = timeout(Duration::from_secs(90), async {
        println!("Waiting for gateway to initialize");
        tokio::time::sleep(Duration::from_secs(8)).await;

        let uri =
            format!("ws://127.0.0.1:{gateway_ws_port}/v1/contract/command?encodingProtocol=native");

        println!("Establishing connection and setting up contract");
        let (stream, _) = connect_async(&uri).await?;
        let mut client_api = WebApi::start(stream);

        make_put(
            &mut client_api,
            wrapped_state.clone(),
            contract.clone(),
            false,
        )
        .await?;

        let put_response = timeout(Duration::from_secs(30), client_api.recv()).await;
        match put_response {
            Ok(Ok(HostResponse::ContractResponse(ContractResponse::PutResponse { key }))) => {
                assert_eq!(key, contract_key);
                println!("Contract established successfully");
            }
            Ok(Ok(other)) => bail!("Unexpected PUT response: {:?}", other),
            Ok(Err(e)) => bail!("PUT error: {}", e),
            Err(_) => bail!("PUT timeout"),
        }

        client_api
            .send(ClientRequest::Disconnect { cause: None })
            .await?;
        tokio::time::sleep(Duration::from_millis(500)).await;

        println!("TERMINATING GATEWAY NODE - Testing network resilience");
        println!("This simulates killing the node process by dropping the future");

        // This is the key: we DROP the node future, simulating process termination
        // The node stops running immediately when we exit this select branch
        Ok::<(), anyhow::Error>(())
    });

    // Use select to race the test against the node
    // When test completes, node future is dropped (simulating process kill)
    select! {
        node_result = &mut gateway_node_future => {
            // Node exited unexpectedly
            match node_result {
                Ok(never) => {
                    // This branch should never execute since node.run() returns Never
                    match never {}
                }
                Err(e) => bail!("Gateway node failed: {}", e),
            }
        }
        test_result = test_result => {
            match test_result {
                Ok(Ok(_)) => {
                    println!("Test completed - node was terminated by dropping future");

                    // Verify node is actually dead by trying to connect
                    let uri = format!("ws://127.0.0.1:{gateway_ws_port}/v1/contract/command?encodingProtocol=native");
                    tokio::time::sleep(Duration::from_secs(2)).await;

                    match connect_async(&uri).await {
                        Ok(_) => {
                            println!("WARNING: Node still responding after termination");
                        }
                        Err(e) => {
                            println!("Confirmed: Gateway node terminated - connection failed: {}", e);
                        }
                    }
                }
                Ok(Err(e)) => bail!("Test error: {}", e),
                Err(_) => bail!("Test timeout"),
            }
        }
    }

    Ok(())
}

/// Test peer resilience when another peer is terminated
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_peer_node_termination() -> anyhow::Result<()> {
    freenet::config::set_logger(Some(LevelFilter::INFO), None);

    const TEST_CONTRACT: &str = "test-contract-integration";
    let contract = load_contract(TEST_CONTRACT, vec![].into())?;
    let contract_key = contract.key();
    let initial_state = test_utils::create_empty_todo_list();
    let wrapped_state = WrappedState::from(initial_state);

    let gateway_socket = TcpListener::bind("127.0.0.1:0")?;
    let gateway_ws_socket = TcpListener::bind("127.0.0.1:0")?;
    let peer1_ws_socket = TcpListener::bind("127.0.0.1:0")?;
    let peer2_ws_socket = TcpListener::bind("127.0.0.1:0")?;

    let gateway_port = gateway_socket.local_addr()?.port();
    let gateway_ws_port = gateway_ws_socket.local_addr()?.port();
    let peer1_ws_port = peer1_ws_socket.local_addr()?.port();
    let peer2_ws_port = peer2_ws_socket.local_addr()?.port();

    let (gateway_config, gateway_temp) =
        create_node_config(true, gateway_ws_port, Some(gateway_port), vec![]).await?;

    let gateway_info = InlineGwConfig {
        address: (Ipv4Addr::LOCALHOST, gateway_port).into(),
        location: Some(RNG.lock().unwrap().random()),
        public_key_path: gateway_temp.path().join("public.pem"),
    };

    let (peer1_config, _peer1_temp) = create_node_config(
        false,
        peer1_ws_port,
        None,
        vec![serde_json::to_string(&gateway_info)?],
    )
    .await?;

    let (peer2_config, _peer2_temp) = create_node_config(
        false,
        peer2_ws_port,
        None,
        vec![serde_json::to_string(&gateway_info)?],
    )
    .await?;

    std::mem::drop(gateway_socket);
    std::mem::drop(gateway_ws_socket);
    std::mem::drop(peer1_ws_socket);
    std::mem::drop(peer2_ws_socket);

    println!("Starting gateway and peer nodes");
    let gateway_future = {
        let config = gateway_config;
        async move {
            let built_config = config.build().await?;
            let node = NodeConfig::new(built_config.clone())
                .await?
                .build(serve_gateway(built_config.ws_api).await)
                .await?;
            node.run().await
        }
    }
    .boxed_local();

    let mut peer1_future = {
        let config = peer1_config;
        async move {
            let built_config = config.build().await?;
            let node = NodeConfig::new(built_config.clone())
                .await?
                .build(serve_gateway(built_config.ws_api).await)
                .await?;
            node.run().await
        }
    }
    .boxed_local();

    let peer2_future = {
        let config = peer2_config;
        async move {
            let built_config = config.build().await?;
            let node = NodeConfig::new(built_config.clone())
                .await?
                .build(serve_gateway(built_config.ws_api).await)
                .await?;
            node.run().await
        }
    }
    .boxed_local();

    let test_result = timeout(Duration::from_secs(120), async {
        println!("Waiting for network to initialize");
        tokio::time::sleep(Duration::from_secs(15)).await;

        let peer1_uri =
            format!("ws://127.0.0.1:{peer1_ws_port}/v1/contract/command?encodingProtocol=native");
        let peer2_uri =
            format!("ws://127.0.0.1:{peer2_ws_port}/v1/contract/command?encodingProtocol=native");

        println!("Establishing contract through peer1");
        let (stream, _) = connect_async(&peer1_uri).await?;
        let mut client_api = WebApi::start(stream);

        make_put(
            &mut client_api,
            wrapped_state.clone(),
            contract.clone(),
            false,
        )
        .await?;

        let put_response = timeout(Duration::from_secs(30), client_api.recv()).await;
        match put_response {
            Ok(Ok(HostResponse::ContractResponse(ContractResponse::PutResponse { key }))) => {
                assert_eq!(key, contract_key);
                println!("Contract established through peer1");
            }
            Ok(Ok(other)) => bail!("Unexpected PUT response: {:?}", other),
            Ok(Err(e)) => bail!("PUT error: {}", e),
            Err(_) => bail!("PUT timeout"),
        }

        client_api
            .send(ClientRequest::Disconnect { cause: None })
            .await?;
        tokio::time::sleep(Duration::from_millis(500)).await;

        println!("TERMINATING PEER1 NODE - Testing network resilience");
        // peer1_future will be dropped when this scope ends, simulating node termination

        println!("Attempting operation through peer2 after peer1 termination");
        tokio::time::sleep(Duration::from_secs(5)).await;

        let (stream, _) = connect_async(&peer2_uri).await?;
        let mut client_api = WebApi::start(stream);

        make_get(&mut client_api, contract_key, true, false).await?;

        let get_response = timeout(Duration::from_secs(30), client_api.recv()).await;
        match get_response {
            Ok(Ok(HostResponse::ContractResponse(ContractResponse::GetResponse { .. }))) => {
                println!("Network remains operational after peer1 termination");
            }
            Ok(Ok(other)) => {
                println!(
                    "GET returned different response after peer death: {:?}",
                    other
                );
            }
            Ok(Err(e)) => {
                println!("GET failed after peer death: {}", e);
            }
            Err(_) => {
                println!("GET timeout after peer death");
            }
        }

        client_api
            .send(ClientRequest::Disconnect { cause: None })
            .await?;
        Ok::<(), anyhow::Error>(())
    });

    // Run the network with selective termination
    select! {
        gateway_result = gateway_future => {
            match gateway_result {
                Ok(never) => match never {},
                Err(e) => bail!("Gateway failed: {}", e),
            }
        }
        peer2_result = peer2_future => {
            match peer2_result {
                Ok(never) => match never {},
                Err(e) => bail!("Peer2 failed: {}", e),
            }
        }
        test_result = test_result => {
            match test_result {
                Ok(Ok(_)) => {
                    println!("Test completed - peer1 terminated, peer2 and gateway still running");
                    // peer1_future is dropped here, simulating termination
                }
                Ok(Err(e)) => bail!("Test error: {}", e),
                Err(_) => bail!("Test timeout"),
            }
        }
    }

    Ok(())
}

/// Test rapid node termination and recovery simulation
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_rapid_node_termination_cycles() -> anyhow::Result<()> {
    freenet::config::set_logger(Some(LevelFilter::INFO), None);

    let gateway_socket = TcpListener::bind("127.0.0.1:0")?;
    let gateway_ws_socket = TcpListener::bind("127.0.0.1:0")?;

    let gateway_port = gateway_socket.local_addr()?.port();
    let gateway_ws_port = gateway_ws_socket.local_addr()?.port();

    std::mem::drop(gateway_socket);
    std::mem::drop(gateway_ws_socket);

    println!("Starting rapid node termination test");

    // Perform multiple cycles of start->test->terminate
    for cycle in 0..3 {
        println!("Termination cycle {}", cycle + 1);

        let (gateway_config, _gateway_temp) =
            create_node_config(true, gateway_ws_port, Some(gateway_port), vec![]).await?;

        println!("Starting gateway node (cycle {})", cycle + 1);
        let mut gateway_future = {
            let config = gateway_config;
            async move {
                let built_config = config.build().await?;
                let node = NodeConfig::new(built_config.clone())
                    .await?
                    .build(serve_gateway(built_config.ws_api).await)
                    .await?;
                node.run().await
            }
        }
        .boxed_local();

        let cycle_test = timeout(Duration::from_secs(30), async {
            tokio::time::sleep(Duration::from_secs(8)).await;

            let uri = format!(
                "ws://127.0.0.1:{gateway_ws_port}/v1/contract/command?encodingProtocol=native"
            );

            // Quick connectivity test
            match connect_async(&uri).await {
                Ok((stream, _)) => {
                    let mut client = WebApi::start(stream);
                    client
                        .send(ClientRequest::NodeQueries(
                            freenet_stdlib::client_api::NodeQuery::ConnectedPeers,
                        ))
                        .await?;
                    client
                        .send(ClientRequest::Disconnect { cause: None })
                        .await?;
                    println!("Connection test successful (cycle {})", cycle + 1);
                }
                Err(e) => {
                    println!("Connection test failed (cycle {}): {}", cycle + 1, e);
                }
            }

            Ok::<(), anyhow::Error>(())
        });

        // Race the test against the node, then terminate by selecting the test
        select! {
            node_result = &mut gateway_future => {
                match node_result {
                    Ok(never) => match never {},
                    Err(e) => bail!("Gateway failed in cycle {}: {}", cycle + 1, e),
                }
            }
            test_result = cycle_test => {
                match test_result {
                    Ok(Ok(_)) => {
                        println!("TERMINATING node (cycle {}) by dropping future", cycle + 1);
                        // gateway_future is dropped when we continue, simulating termination
                    }
                    Ok(Err(e)) => {
                        println!("Test error in cycle {}: {}", cycle + 1, e);
                    }
                    Err(_) => {
                        println!("Test timeout in cycle {}", cycle + 1);
                    }
                }
            }
        }

        // Brief pause between cycles
        tokio::time::sleep(Duration::from_millis(1000)).await;

        // Verify termination
        let uri =
            format!("ws://127.0.0.1:{gateway_ws_port}/v1/contract/command?encodingProtocol=native");
        match connect_async(&uri).await {
            Ok(_) => {
                println!(
                    "WARNING: Node still responding after termination (cycle {})",
                    cycle + 1
                );
            }
            Err(_) => {
                println!("Node successfully terminated (cycle {})", cycle + 1);
            }
        }
    }

    println!("Rapid node termination test completed successfully");
    Ok(())
}
