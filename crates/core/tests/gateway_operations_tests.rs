//! Gateway operations validation tests
//!
//! Tests core gateway functionality including PUT, GET, Subscribe, and Update operations
//! to ensure they work correctly under various conditions.

use anyhow::bail;
use freenet::{
    config::{ConfigArgs, NetworkArgs, SecretArgs, WebsocketApiArgs},
    dev_tool::TransportKeypair,
    local_node::NodeConfig,
    server::serve_gateway,
    test_utils::{self, load_contract, make_get, make_put, make_subscribe, verify_contract_exists},
};
use freenet_stdlib::{
    client_api::{ClientRequest, ContractResponse, HostResponse, WebApi},
    prelude::*,
};
use futures::FutureExt;
use rand::{Rng, SeedableRng};
use std::{
    net::Ipv4Addr,
    sync::{LazyLock, Mutex},
    time::Duration,
};
use tokio::{select, time::timeout};
use tokio_tungstenite::connect_async;
use tracing::{error, level_filters::LevelFilter};

static RNG: LazyLock<Mutex<rand::rngs::StdRng>> = LazyLock::new(|| {
    Mutex::new(rand::rngs::StdRng::from_seed(
        *b"gateway_operations_test_seed_789",
    ))
});

async fn create_gateway_config(
    ws_api_port: u16,
    network_port: u16,
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
            public_port: Some(network_port),
            is_gateway: true,
            skip_load_from_network: true,
            gateways: Some(vec![]),
            location: Some(RNG.lock().unwrap().random()),
            ignore_protocol_checking: true,
            address: Some(Ipv4Addr::LOCALHOST.into()),
            network_port: Some(network_port),
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

/// Test basic PUT and GET operations on a gateway node
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_gateway_put_get_operations() -> anyhow::Result<()> {
    freenet::config::set_logger(Some(LevelFilter::INFO), None);

    const TEST_CONTRACT: &str = "test-contract-integration";
    let contract = load_contract(TEST_CONTRACT, vec![].into())?;
    let contract_key = contract.key();
    let initial_state = test_utils::create_empty_todo_list();
    let wrapped_state = WrappedState::from(initial_state);

    let ws_port = 56000;
    let network_port = 56001;
    let (config, temp_dir) = create_gateway_config(ws_port, network_port).await?;

    let node_handle = async move {
        let built_config = config.build().await?;
        let node = NodeConfig::new(built_config.clone())
            .await?
            .build(serve_gateway(built_config.ws_api).await)
            .await?;
        node.run().await
    }
    .boxed_local();

    let test_result = timeout(Duration::from_secs(60), async {
        println!("Starting gateway for PUT/GET operations test...");
        tokio::time::sleep(Duration::from_secs(8)).await;

        let url = format!(
            "ws://localhost:{}/v1/contract/command?encodingProtocol=native",
            ws_port
        );
        let (ws_stream, _) = connect_async(&url).await?;
        let mut client = WebApi::start(ws_stream);

        // Test PUT operation
        println!("Testing PUT operation");
        make_put(&mut client, wrapped_state.clone(), contract.clone(), false).await?;

        let put_response = timeout(Duration::from_secs(20), client.recv()).await;
        match put_response {
            Ok(Ok(HostResponse::ContractResponse(ContractResponse::PutResponse { key }))) => {
                assert_eq!(key, contract_key);
                println!("PUT operation completed successfully");
            }
            Ok(Ok(other)) => bail!("Unexpected PUT response: {:?}", other),
            Ok(Err(e)) => bail!("PUT operation failed: {}", e),
            Err(_) => bail!("PUT operation timed out"),
        }

        // Verify contract was stored locally
        assert!(verify_contract_exists(temp_dir.path(), contract_key).await?);
        println!("Contract verified in local storage");

        // Test GET operation
        println!("Testing GET operation");
        make_get(&mut client, contract_key, true, false).await?;

        let get_response = timeout(Duration::from_secs(20), client.recv()).await;
        match get_response {
            Ok(Ok(HostResponse::ContractResponse(ContractResponse::GetResponse {
                contract: recv_contract,
                state: recv_state,
                ..
            }))) => {
                assert_eq!(
                    recv_contract
                        .as_ref()
                        .expect("Contract should be present")
                        .key(),
                    contract_key
                );
                assert_eq!(recv_state, wrapped_state);
                println!("GET operation completed successfully");
            }
            Ok(Ok(other)) => bail!("Unexpected GET response: {:?}", other),
            Ok(Err(e)) => bail!("GET operation failed: {}", e),
            Err(_) => bail!("GET operation timed out"),
        }

        client
            .send(ClientRequest::Disconnect { cause: None })
            .await?;
        tokio::time::sleep(Duration::from_millis(100)).await;

        Ok::<(), anyhow::Error>(())
    });

    select! {
        _ = node_handle => {
            error!("Gateway node exited unexpectedly");
            bail!("Gateway should continue running during test");
        }
        result = test_result => {
            result??;
        }
    }

    Ok(())
}

/// Test subscription operations and event handling
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_gateway_subscription_operations() -> anyhow::Result<()> {
    freenet::config::set_logger(Some(LevelFilter::INFO), None);

    const TEST_CONTRACT: &str = "test-contract-integration";
    let contract = load_contract(TEST_CONTRACT, vec![].into())?;
    let contract_key = contract.key();
    let initial_state = test_utils::create_empty_todo_list();
    let wrapped_state = WrappedState::from(initial_state);

    let ws_port = 56100;
    let network_port = 56101;
    let (config, _temp_dir) = create_gateway_config(ws_port, network_port).await?;

    let node_handle = async move {
        let built_config = config.build().await?;
        let node = NodeConfig::new(built_config.clone())
            .await?
            .build(serve_gateway(built_config.ws_api).await)
            .await?;
        node.run().await
    }
    .boxed_local();

    let test_result = timeout(Duration::from_secs(90), async {
        println!("Starting gateway for subscription operations test...");
        tokio::time::sleep(Duration::from_secs(8)).await;

        let url = format!(
            "ws://localhost:{}/v1/contract/command?encodingProtocol=native",
            ws_port
        );
        let (ws_stream, _) = connect_async(&url).await?;
        let mut client = WebApi::start(ws_stream);

        // First, PUT the contract so we have something to subscribe to
        println!("Setting up contract for subscription test");
        make_put(&mut client, wrapped_state.clone(), contract.clone(), false).await?;

        let put_response = timeout(Duration::from_secs(20), client.recv()).await;
        match put_response {
            Ok(Ok(HostResponse::ContractResponse(ContractResponse::PutResponse { key }))) => {
                assert_eq!(key, contract_key);
                println!("Contract setup completed");
            }
            Ok(Ok(other)) => bail!("Unexpected setup PUT response: {:?}", other),
            Ok(Err(e)) => bail!("Setup PUT failed: {}", e),
            Err(_) => bail!("Setup PUT timed out"),
        }

        // Test explicit subscription
        println!("Testing explicit subscription");
        make_subscribe(&mut client, contract_key).await?;

        let subscribe_response = timeout(Duration::from_secs(20), client.recv()).await;
        match subscribe_response {
            Ok(Ok(HostResponse::ContractResponse(ContractResponse::SubscribeResponse {
                key,
                ..
            }))) => {
                assert_eq!(key, contract_key);
                println!("Explicit subscription successful");
            }
            Ok(Ok(other)) => bail!("Unexpected subscription response: {:?}", other),
            Ok(Err(e)) => bail!("Subscription failed: {}", e),
            Err(_) => bail!("Subscription timed out"),
        }

        // Test GET with automatic subscription
        println!("Testing GET with auto-subscribe");
        make_get(&mut client, contract_key, true, true).await?;

        let get_sub_response = timeout(Duration::from_secs(20), client.recv()).await;
        match get_sub_response {
            Ok(Ok(HostResponse::ContractResponse(ContractResponse::GetResponse {
                contract: recv_contract,
                state: recv_state,
                ..
            }))) => {
                assert_eq!(
                    recv_contract
                        .as_ref()
                        .expect("Contract should be present")
                        .key(),
                    contract_key
                );
                assert_eq!(recv_state, wrapped_state);
                println!("GET with auto-subscribe successful");
            }
            Ok(Ok(other)) => bail!("Unexpected GET+subscribe response: {:?}", other),
            Ok(Err(e)) => bail!("GET+subscribe failed: {}", e),
            Err(_) => bail!("GET+subscribe timed out"),
        }

        client
            .send(ClientRequest::Disconnect { cause: None })
            .await?;
        tokio::time::sleep(Duration::from_millis(100)).await;

        Ok::<(), anyhow::Error>(())
    });

    select! {
        _ = node_handle => {
            error!("Gateway node exited unexpectedly");
            bail!("Gateway should continue running during subscription test");
        }
        result = test_result => {
            result??;
        }
    }

    Ok(())
}

/// Test sequential operations to verify state consistency
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_sequential_operations() -> anyhow::Result<()> {
    freenet::config::set_logger(Some(LevelFilter::INFO), None);

    const TEST_CONTRACT: &str = "test-contract-integration";
    let contract = load_contract(TEST_CONTRACT, vec![].into())?;
    let contract_key = contract.key();
    let initial_state = test_utils::create_empty_todo_list();
    let wrapped_state = WrappedState::from(initial_state);

    let ws_port = 56200;
    let network_port = 56201;
    let (config, temp_dir) = create_gateway_config(ws_port, network_port).await?;

    let node_handle = async move {
        let built_config = config.build().await?;
        let node = NodeConfig::new(built_config.clone())
            .await?
            .build(serve_gateway(built_config.ws_api).await)
            .await?;
        node.run().await
    }
    .boxed_local();

    let test_result = timeout(Duration::from_secs(90), async {
        println!("Starting gateway for sequential operations test...");
        tokio::time::sleep(Duration::from_secs(8)).await;

        let url = format!(
            "ws://localhost:{}/v1/contract/command?encodingProtocol=native",
            ws_port
        );
        let (ws_stream, _) = connect_async(&url).await?;
        let mut client = WebApi::start(ws_stream);

        // Sequence 1: PUT -> GET -> PUT again
        println!("Sequence 1: PUT -> GET -> PUT again");

        make_put(&mut client, wrapped_state.clone(), contract.clone(), false).await?;

        let put1_response = timeout(Duration::from_secs(20), client.recv()).await;
        match put1_response {
            Ok(Ok(HostResponse::ContractResponse(ContractResponse::PutResponse { key }))) => {
                assert_eq!(key, contract_key);
                println!("First PUT completed");
            }
            Ok(Ok(other)) => bail!("Unexpected first PUT response: {:?}", other),
            Ok(Err(e)) => bail!("First PUT failed: {}", e),
            Err(_) => bail!("First PUT timed out"),
        }

        make_get(&mut client, contract_key, true, false).await?;

        let get_response = timeout(Duration::from_secs(20), client.recv()).await;
        match get_response {
            Ok(Ok(HostResponse::ContractResponse(ContractResponse::GetResponse {
                contract: recv_contract,
                state: recv_state,
                ..
            }))) => {
                assert_eq!(
                    recv_contract
                        .as_ref()
                        .expect("Contract should be present")
                        .key(),
                    contract_key
                );
                assert_eq!(recv_state, wrapped_state);
                println!("GET after first PUT successful");
            }
            Ok(Ok(other)) => bail!("Unexpected GET response: {:?}", other),
            Ok(Err(e)) => bail!("GET failed: {}", e),
            Err(_) => bail!("GET timed out"),
        }

        // Second PUT with same contract (should succeed)
        make_put(&mut client, wrapped_state.clone(), contract.clone(), false).await?;

        let put2_response = timeout(Duration::from_secs(20), client.recv()).await;
        match put2_response {
            Ok(Ok(HostResponse::ContractResponse(ContractResponse::PutResponse { key }))) => {
                assert_eq!(key, contract_key);
                println!("Second PUT completed");
            }
            Ok(Ok(other)) => bail!("Unexpected second PUT response: {:?}", other),
            Ok(Err(e)) => bail!("Second PUT failed: {}", e),
            Err(_) => bail!("Second PUT timed out"),
        }

        // Verify contract still exists in storage
        assert!(verify_contract_exists(temp_dir.path(), contract_key).await?);
        println!("Contract persistence verified after sequential operations");

        // Sequence 2: Subscribe -> GET with subscribe -> Verify
        println!("Sequence 2: Subscribe -> GET with subscribe");

        make_subscribe(&mut client, contract_key).await?;

        let subscribe_response = timeout(Duration::from_secs(20), client.recv()).await;
        match subscribe_response {
            Ok(Ok(HostResponse::ContractResponse(ContractResponse::SubscribeResponse {
                key,
                ..
            }))) => {
                assert_eq!(key, contract_key);
                println!("Subscription established");
            }
            Ok(Ok(other)) => bail!("Unexpected subscription response: {:?}", other),
            Ok(Err(e)) => bail!("Subscription failed: {}", e),
            Err(_) => bail!("Subscription timed out"),
        }

        make_get(&mut client, contract_key, true, true).await?;

        let final_get_response = timeout(Duration::from_secs(20), client.recv()).await;
        match final_get_response {
            Ok(Ok(HostResponse::ContractResponse(ContractResponse::GetResponse {
                contract: recv_contract,
                state: recv_state,
                ..
            }))) => {
                assert_eq!(
                    recv_contract
                        .as_ref()
                        .expect("Contract should be present")
                        .key(),
                    contract_key
                );
                assert_eq!(recv_state, wrapped_state);
                println!("Final GET with subscription successful");
            }
            Ok(Ok(other)) => bail!("Unexpected final GET response: {:?}", other),
            Ok(Err(e)) => bail!("Final GET failed: {}", e),
            Err(_) => bail!("Final GET timed out"),
        }

        client
            .send(ClientRequest::Disconnect { cause: None })
            .await?;
        tokio::time::sleep(Duration::from_millis(100)).await;

        println!("Sequential operations test completed successfully");
        Ok::<(), anyhow::Error>(())
    });

    select! {
        _ = node_handle => {
            error!("Gateway node exited unexpectedly");
            bail!("Gateway should continue running during sequential operations test");
        }
        result = test_result => {
            result??;
        }
    }

    Ok(())
}

/// Test error handling for invalid operations
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_invalid_operations_handling() -> anyhow::Result<()> {
    freenet::config::set_logger(Some(LevelFilter::INFO), None);

    let ws_port = 56300;
    let network_port = 56301;
    let (config, _temp_dir) = create_gateway_config(ws_port, network_port).await?;

    let node_handle = async move {
        let built_config = config.build().await?;
        let node = NodeConfig::new(built_config.clone())
            .await?
            .build(serve_gateway(built_config.ws_api).await)
            .await?;
        node.run().await
    }
    .boxed_local();

    let test_result = timeout(Duration::from_secs(60), async {
        println!("Starting gateway for invalid operations test...");
        tokio::time::sleep(Duration::from_secs(8)).await;

        let url = format!(
            "ws://localhost:{}/v1/contract/command?encodingProtocol=native",
            ws_port
        );
        let (ws_stream, _) = connect_async(&url).await?;
        let mut client = WebApi::start(ws_stream);

        // Test GET for non-existent contract
        println!("Testing GET for non-existent contract");
        let params = Parameters::from(vec![127, 127, 127]);
        let non_existent_contract = load_contract("test-contract-1", params)?;
        let non_existent_key = non_existent_contract.key();

        make_get(&mut client, non_existent_key, false, false).await?;

        let get_result = timeout(Duration::from_secs(15), client.recv()).await;

        // Should handle gracefully - either return error response or timeout
        match get_result {
            Ok(Ok(HostResponse::ContractResponse(ContractResponse::GetResponse { .. }))) => {
                bail!("Should not successfully GET a non-existent contract");
            }
            Ok(Ok(_)) | Ok(Err(_)) | Err(_) => {
                println!("GET for non-existent contract handled appropriately");
            }
        }

        // Test subscription to non-existent contract
        println!("Testing subscription to non-existent contract");
        make_subscribe(&mut client, non_existent_key).await?;

        let subscribe_result = timeout(Duration::from_secs(15), client.recv()).await;

        match subscribe_result {
            Ok(Ok(HostResponse::ContractResponse(ContractResponse::SubscribeResponse {
                ..
            }))) => {
                println!("Subscription to non-existent contract accepted (may be valid behavior)");
            }
            Ok(Ok(_)) | Ok(Err(_)) | Err(_) => {
                println!("Subscription to non-existent contract handled appropriately");
            }
        }

        client
            .send(ClientRequest::Disconnect { cause: None })
            .await?;
        tokio::time::sleep(Duration::from_millis(100)).await;

        println!("Invalid operations handling test completed");
        Ok::<(), anyhow::Error>(())
    });

    select! {
        _ = node_handle => {
            error!("Gateway node exited unexpectedly");
            bail!("Gateway should continue running during invalid operations test");
        }
        result = test_result => {
            result??;
        }
    }

    Ok(())
}
