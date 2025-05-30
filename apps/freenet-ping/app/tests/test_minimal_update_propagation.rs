/// Minimal test to debug update propagation issues
///
/// Network topology:
/// - 2 gateways (for redundancy)
/// - 2 regular nodes
/// - Full connectivity (no blocking)
///
/// Test flow:
/// 1. Node 0 publishes a contract
/// 2. All nodes retrieve and subscribe to the contract
/// 3. Node 0 sends an update
/// 4. Verify Node 1 receives the update
mod common;

use anyhow::anyhow;
use freenet::{local_node::NodeConfig, server::serve_gateway};
use freenet_ping_app::ping_client::wait_for_put_response;
use freenet_ping_types::{Ping, PingContractOptions};
use freenet_stdlib::{
    client_api::{ClientRequest, ContractRequest, ContractResponse, HostResponse, WebApi},
    prelude::*,
};
use futures::FutureExt;
use std::{net::TcpListener, path::PathBuf, time::Duration};
use testresult::TestResult;
use tokio::{select, time::timeout};
use tokio_tungstenite::connect_async;
use tracing::level_filters::LevelFilter;

use common::{base_node_test_config, gw_config_from_path, APP_TAG, PACKAGE_DIR, PATH_TO_CONTRACT};

#[tokio::test(flavor = "multi_thread")]
async fn test_minimal_update_propagation() -> TestResult {
    freenet::config::set_logger(Some(LevelFilter::DEBUG), None);

    const NUM_GATEWAYS: usize = 2;
    const NUM_NODES: usize = 2;

    println!("🔧 Starting minimal update propagation test");
    println!("   Gateways: {}", NUM_GATEWAYS);
    println!("   Nodes: {}", NUM_NODES);
    println!("   Connectivity: Full (no blocking)\n");

    // Setup sockets
    let mut gateway_sockets = Vec::with_capacity(NUM_GATEWAYS);
    let mut ws_api_gateway_sockets = Vec::with_capacity(NUM_GATEWAYS);

    for _ in 0..NUM_GATEWAYS {
        gateway_sockets.push(TcpListener::bind("127.0.0.1:0")?);
        ws_api_gateway_sockets.push(TcpListener::bind("127.0.0.1:0")?);
    }

    let mut ws_api_node_sockets = Vec::with_capacity(NUM_NODES);
    for _ in 0..NUM_NODES {
        ws_api_node_sockets.push(TcpListener::bind("127.0.0.1:0")?);
    }

    // Configure gateways
    let mut gateway_info = Vec::new();
    let mut ws_api_ports_gw = Vec::new();
    let mut gateway_configs = Vec::with_capacity(NUM_GATEWAYS);
    let mut gateway_presets = Vec::with_capacity(NUM_GATEWAYS);

    for i in 0..NUM_GATEWAYS {
        let (cfg, preset) = base_node_test_config(
            true,
            vec![],
            Some(gateway_sockets[i].local_addr()?.port()),
            ws_api_gateway_sockets[i].local_addr()?.port(),
            &format!("gw_minimal_{}", i),
            None,
            None, // No blocked addresses - full connectivity
        )
        .await?;

        let public_port = cfg.network_api.public_port.unwrap();
        let path = preset.temp_dir.path().to_path_buf();
        let config_info = gw_config_from_path(public_port, &path)?;

        println!("Gateway {} configured", i);
        ws_api_ports_gw.push(cfg.ws_api.ws_api_port.unwrap());
        gateway_info.push(config_info);
        gateway_configs.push(cfg);
        gateway_presets.push(preset);
    }

    // Configure nodes
    let serialized_gateways: Vec<String> = gateway_info
        .iter()
        .map(|info| serde_json::to_string(info).unwrap())
        .collect();

    let mut ws_api_ports_nodes = Vec::new();
    let mut node_configs = Vec::with_capacity(NUM_NODES);
    let mut node_presets = Vec::with_capacity(NUM_NODES);

    for i in 0..NUM_NODES {
        let (cfg, preset) = base_node_test_config(
            false,
            serialized_gateways.clone(),
            None,
            ws_api_node_sockets[i].local_addr()?.port(),
            &format!("node_minimal_{}", i),
            None,
            None, // No blocked addresses - full connectivity
        )
        .await?;

        println!("Node {} configured", i);
        ws_api_ports_nodes.push(cfg.ws_api.ws_api_port.unwrap());
        node_configs.push(cfg);
        node_presets.push(preset);
    }

    // Free ports
    std::mem::drop(gateway_sockets);
    std::mem::drop(ws_api_gateway_sockets);
    std::mem::drop(ws_api_node_sockets);

    // Start all nodes
    println!("\n🚀 Starting network nodes...");

    let mut gateway_futures = Vec::with_capacity(NUM_GATEWAYS);
    for config in gateway_configs {
        let gateway_future = async {
            let config = config.build().await?;
            let node = NodeConfig::new(config.clone())
                .await?
                .build(serve_gateway(config.ws_api).await)
                .await?;
            node.run().await
        }
        .boxed_local();
        gateway_futures.push(gateway_future);
    }

    tokio::time::sleep(Duration::from_secs(2)).await;

    let mut node_futures = Vec::with_capacity(NUM_NODES);
    for config in node_configs {
        let node_future = async {
            let config = config.build().await?;
            let node = NodeConfig::new(config.clone())
                .await?
                .build(serve_gateway(config.ws_api).await)
                .await?;
            node.run().await
        }
        .boxed_local();
        node_futures.push(node_future);
    }

    let test = tokio::time::timeout(Duration::from_secs(120), async {
        // Wait for nodes to start
        println!("⏳ Waiting for network to stabilize...");
        tokio::time::sleep(Duration::from_secs(10)).await;

        // Connect to all nodes
        println!("\n📡 Connecting to nodes...");
        let mut gateway_clients = Vec::with_capacity(NUM_GATEWAYS);
        for (i, port) in ws_api_ports_gw.iter().enumerate() {
            let uri = format!(
                "ws://127.0.0.1:{}/v1/contract/command?encodingProtocol=native",
                port
            );
            let (stream, _) = connect_async(&uri).await?;
            let client = WebApi::start(stream);
            gateway_clients.push(client);
            println!("✓ Connected to gateway {}", i);
        }

        let mut node_clients = Vec::with_capacity(NUM_NODES);
        for (i, port) in ws_api_ports_nodes.iter().enumerate() {
            let uri = format!(
                "ws://127.0.0.1:{}/v1/contract/command?encodingProtocol=native",
                port
            );
            let (stream, _) = connect_async(&uri).await?;
            let client = WebApi::start(stream);
            node_clients.push(client);
            println!("✓ Connected to node {}", i);
        }

        // Load ping contract
        let path_to_code = PathBuf::from(PACKAGE_DIR).join(PATH_TO_CONTRACT);
        let ping_options = PingContractOptions {
            frequency: Duration::from_secs(2),
            ttl: Duration::from_secs(60),
            tag: APP_TAG.to_string(),
            code_key: "".to_string(),
        };
        let params = Parameters::from(serde_json::to_vec(&ping_options).unwrap());
        let container = common::load_contract(&path_to_code, params)?;
        let contract_key = container.key();

        // Step 1: Node 0 publishes the contract
        println!("\n📤 Step 1: Node 0 publishes contract...");
        let ping = Ping::default();
        let serialized = serde_json::to_vec(&ping)?;
        let wrapped_state = WrappedState::new(serialized);

        node_clients[0]
            .send(ClientRequest::ContractOp(ContractRequest::Put {
                contract: container.clone(),
                state: wrapped_state.clone(),
                related_contracts: RelatedContracts::new(),
                subscribe: false,
            }))
            .await?;

        let _key = timeout(
            Duration::from_secs(30),
            wait_for_put_response(&mut node_clients[0], &contract_key),
        )
        .await
        .map_err(|_| anyhow!("Put request timed out"))?
        .map_err(anyhow::Error::msg)?;

        println!("✓ Contract published successfully");

        // Wait for propagation
        tokio::time::sleep(Duration::from_secs(5)).await;

        // Step 2: All nodes get and subscribe to the contract
        println!("\n📥 Step 2: All nodes retrieve and subscribe to contract...");

        // Get contract on all nodes
        for (i, client) in node_clients.iter_mut().enumerate() {
            client
                .send(ClientRequest::ContractOp(ContractRequest::Get {
                    key: contract_key,
                    return_contract_code: true,
                    subscribe: false,
                }))
                .await?;
        }

        // Wait for get responses
        for (i, client) in node_clients.iter_mut().enumerate() {
            match timeout(Duration::from_secs(10), client.recv()).await {
                Ok(Ok(HostResponse::ContractResponse(ContractResponse::GetResponse {
                    key,
                    ..
                }))) if key == contract_key => {
                    println!("✓ Node {} retrieved contract", i);
                }
                _ => {
                    println!("✗ Node {} failed to retrieve contract", i);
                    return Err(anyhow!("Node {} couldn't get contract", i));
                }
            }
        }

        // Subscribe all nodes
        for (i, client) in node_clients.iter_mut().enumerate() {
            client
                .send(ClientRequest::ContractOp(ContractRequest::Subscribe {
                    key: contract_key,
                    summary: None,
                }))
                .await?;
        }

        // Wait for subscribe responses
        for (i, client) in node_clients.iter_mut().enumerate() {
            match timeout(Duration::from_secs(10), client.recv()).await {
                Ok(Ok(HostResponse::ContractResponse(ContractResponse::SubscribeResponse {
                    key,
                    subscribed,
                }))) if key == contract_key => {
                    if subscribed {
                        println!("✓ Node {} subscribed to contract", i);
                    } else {
                        println!("✗ Node {} failed to subscribe", i);
                        return Err(anyhow!("Node {} couldn't subscribe", i));
                    }
                }
                _ => {
                    println!("✗ Node {} subscription response error", i);
                    return Err(anyhow!("Node {} subscription failed", i));
                }
            }
        }

        // Step 3: Node 0 sends an update
        println!("\n📝 Step 3: Node 0 sends update...");
        let update_tag = "test-update-from-node-0".to_string();
        let mut update_ping = Ping::default();
        update_ping.insert(update_tag.clone());

        node_clients[0]
            .send(ClientRequest::ContractOp(ContractRequest::Update {
                key: contract_key,
                data: UpdateData::Delta(StateDelta::from(
                    serde_json::to_vec(&update_ping).unwrap(),
                )),
            }))
            .await?;

        // Wait for update response
        match timeout(Duration::from_secs(10), node_clients[0].recv()).await {
            Ok(Ok(HostResponse::ContractResponse(ContractResponse::UpdateResponse {
                key,
                ..
            }))) if key == contract_key => {
                println!("✓ Update sent successfully");
            }
            _ => {
                println!("✗ Update failed");
                return Err(anyhow!("Update operation failed"));
            }
        }

        // Step 4: Wait and check if Node 1 received the update
        println!("\n⏳ Waiting for update propagation...");
        tokio::time::sleep(Duration::from_secs(10)).await;

        println!("\n🔍 Step 4: Checking if Node 1 received update...");
        node_clients[1]
            .send(ClientRequest::ContractOp(ContractRequest::Get {
                key: contract_key,
                return_contract_code: false,
                subscribe: false,
            }))
            .await?;

        match timeout(Duration::from_secs(10), node_clients[1].recv()).await {
            Ok(Ok(HostResponse::ContractResponse(ContractResponse::GetResponse {
                key,
                state,
                ..
            }))) if key == contract_key => match serde_json::from_slice::<Ping>(&state) {
                Ok(ping_state) => {
                    if ping_state.contains_key(&update_tag) {
                        println!("✅ SUCCESS: Node 1 received the update!");
                        println!("   Update tag '{}' found in state", update_tag);
                    } else {
                        println!("❌ FAILURE: Node 1 did NOT receive the update!");
                        println!("   State keys: {:?}", ping_state.keys().collect::<Vec<_>>());
                        return Err(anyhow!("Update did not propagate to Node 1"));
                    }
                }
                Err(e) => {
                    println!("❌ Failed to deserialize state: {}", e);
                    return Err(anyhow!("State deserialization failed"));
                }
            },
            _ => {
                println!("❌ Failed to get state from Node 1");
                return Err(anyhow!("Couldn't retrieve state from Node 1"));
            }
        }

        // Also check gateways
        println!("\n🔍 Checking gateway states...");
        for (i, client) in gateway_clients.iter_mut().enumerate() {
            client
                .send(ClientRequest::ContractOp(ContractRequest::Get {
                    key: contract_key,
                    return_contract_code: false,
                    subscribe: false,
                }))
                .await?;

            match timeout(Duration::from_secs(10), client.recv()).await {
                Ok(Ok(HostResponse::ContractResponse(ContractResponse::GetResponse {
                    key,
                    state,
                    ..
                }))) if key == contract_key => match serde_json::from_slice::<Ping>(&state) {
                    Ok(ping_state) => {
                        if ping_state.contains_key(&update_tag) {
                            println!("✓ Gateway {} has the update", i);
                        } else {
                            println!("✗ Gateway {} does NOT have the update", i);
                        }
                    }
                    Err(_) => println!("✗ Gateway {} state deserialization failed", i),
                },
                _ => println!("✗ Gateway {} get failed", i),
            }
        }

        println!("\n✅ Test completed");
        Ok::<_, anyhow::Error>(())
    });

    // Run test with node monitoring
    let mut all_futures = Vec::new();
    all_futures.extend(gateway_futures);
    all_futures.extend(node_futures);

    let mut test_future = Box::pin(test);

    loop {
        let select_all_futures = futures::future::select_all(all_futures);

        select! {
            (result, _index, remaining) = select_all_futures => {
                match result {
                    Err(err) => {
                        return Err(anyhow!("Node failed: {}", err).into());
                    }
                    Ok(_) => {
                        all_futures = remaining;
                        if all_futures.is_empty() {
                            println!("All nodes completed before test finished");
                            break;
                        }
                    }
                }
            }
            r = &mut test_future => {
                r??;
                break;
            }
        }
    }

    Ok(())
}
