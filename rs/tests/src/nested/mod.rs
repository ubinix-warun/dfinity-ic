use std::str::FromStr;

use canister_test::PrincipalId;
use slog::info;

use crate::driver::bootstrap::setup_and_start_nested_vms;
use crate::driver::farm::Farm;
use crate::driver::ic::InternetComputer;
use crate::driver::nested::{NestedNode, NestedVms};
use crate::driver::resource::{allocate_resources, get_resource_request_for_nested_nodes};
use crate::driver::test_env::{HasIcPrepDir, TestEnv, TestEnvAttribute};
use crate::driver::test_env_api::*;
use crate::driver::test_setup::GroupSetup;
use crate::nns::add_nodes_to_subnet;
use crate::orchestrator::utils::rw_message::install_nns_and_check_progress;
use crate::util::{block_on, get_nns_node};
use ic_registry_subnet_type::SubnetType;
use ic_types::hostos_version::HostosVersion;

mod util;
use util::{check_hostos_version, elect_hostos_version, update_nodes_hostos_version};

const HOST_VM_NAME: &str = "host-1";

/// Prepare the environment for nested tests.
/// SetupOS -> HostOS -> GuestOS
pub fn config(env: TestEnv) {
    let logger = env.logger();
    let farm_url = env.get_farm_url().expect("Unable to get Farm url.");
    let farm = Farm::new(farm_url, logger.clone());
    let group_setup = GroupSetup::read_attribute(&env);
    let group_name: String = group_setup.infra_group_name;
    let principal =
        PrincipalId::from_str("7532g-cd7sa-3eaay-weltl-purxe-qliyt-hfuto-364ru-b3dsz-kw5uz-kqe")
            .unwrap();

    // Setup "testnet"
    InternetComputer::new()
        .add_fast_single_node_subnet(SubnetType::System)
        .with_node_provider(principal)
        .with_node_operator(principal)
        .setup_and_start(&env)
        .expect("failed to setup IC under test");

    let nns_node = get_nns_node(&env.topology_snapshot());
    let nns_url = nns_node.get_public_url();
    let nns_public_key =
        std::fs::read_to_string(env.prep_dir("").unwrap().root_public_key_path()).unwrap();

    // Setup nested GuestOS
    let nodes = vec![NestedNode::new(HOST_VM_NAME.to_string())];

    let res_request = get_resource_request_for_nested_nodes(&nodes, &env, &group_name, &farm)
        .expect("Failed to build resource request for nested test.");
    let res_group = allocate_resources(&farm, &res_request)
        .expect("Failed to allocate resources for nested test.");

    for (name, vm) in res_group.vms.iter() {
        env.write_nested_vm(name, vm)
            .expect("Unable to write nested VM.");
    }

    setup_and_start_nested_vms(&nodes, &env, &farm, &group_name, &nns_url, &nns_public_key)
        .expect("Unable to start nested VMs.");

    install_nns_and_check_progress(env.topology_snapshot());
}

/// Allow the nested GuestOS to install and launch, and check that it can
/// successfully join the testnet.
pub fn registration(env: TestEnv) {
    let logger = env.logger();

    // Check that there are initially no unassigned nodes.
    let num_unassigned_nodes = block_on(
        env.topology_snapshot()
            .block_for_min_registry_version(ic_types::RegistryVersion::from(1)),
    )
    .unwrap()
    .unassigned_nodes()
    .count();
    assert_eq!(num_unassigned_nodes, 0);

    info!(logger, "Waiting for SetupOS to install...");
    std::thread::sleep(std::time::Duration::from_secs(7 * 60));

    // If the node is able to join successfully, the registry will be updated,
    // and the new node ID will enter the unassigned pool.
    info!(logger, "Waiting for node to join...");
    let num_unassigned_nodes = block_on(env.topology_snapshot().block_for_newer_registry_version())
        .unwrap()
        .unassigned_nodes()
        .count();
    assert_eq!(num_unassigned_nodes, 1);
}

/// Upgrade each HostOS VM to the test version, and verify that each is
/// healthy before and after the upgrade.
pub fn upgrade(env: TestEnv) {
    let logger = env.logger();

    let original_version = env
        .read_dependency_from_env_to_string("ENV_DEPS__IC_VERSION_FILE")
        .expect("tip-of-branch IC version");

    let target_version = HostosVersion::try_from(format!("{original_version}-test")).unwrap();
    let url = env.get_hostos_update_img_test_url().unwrap();
    let sha256 = env.get_hostos_update_img_test_sha256().unwrap();

    // Choose a node from the nns subnet
    let nns_subnet = env.topology_snapshot().root_subnet();
    let nns_node = nns_subnet.nodes().next().unwrap();

    let host = env
        .get_nested_vm(HOST_VM_NAME)
        .expect("Unable to find HostOS node.");

    info!(logger, "Waiting for SetupOS to install...");
    std::thread::sleep(std::time::Duration::from_secs(7 * 60));

    // Check version
    info!(
        logger,
        "Checking version via SSH on HostOS: '{}'",
        host.get_vm().expect("Unable to get HostOS VM.").ipv6
    );
    let original_version = check_hostos_version(&host);
    info!(logger, "Version found is: '{}'", original_version);

    // Add the node to a subnet to start the replica
    let node_id = block_on(env.topology_snapshot().block_for_newer_registry_version())
        .unwrap()
        .unassigned_nodes()
        .next()
        .unwrap()
        .node_id;

    info!(
        logger,
        "Adding node '{}' to subnet '{}'", node_id, nns_subnet.subnet_id
    );
    block_on(add_nodes_to_subnet(
        nns_node.get_public_url(),
        nns_subnet.subnet_id,
        &[node_id],
    ))
    .unwrap();
    host.await_status_is_healthy().unwrap();

    // Elect target HostOS version
    info!(logger, "Electing target HostOS version '{target_version}' with sha256 '{sha256}' and upgrade urls: '{url}'");
    block_on(elect_hostos_version(
        &nns_node,
        &target_version,
        &sha256,
        vec![url.to_string()],
    ));
    info!(logger, "Elected target HostOS version");

    info!(
        logger,
        "Upgrading node '{}' to '{}'", node_id, target_version
    );
    block_on(update_nodes_hostos_version(
        &nns_node,
        &target_version,
        vec![node_id],
    ));

    // The HostOS upgrade is applied with a reboot to the host VM, so we will
    // lose access to the replica. Ensure that it comes back successfully in
    // the new system.
    info!(logger, "Waiting for the upgrade to apply...");
    host.await_status_is_unavailable().unwrap();
    info!(logger, "Waiting for the replica to come back healthy...");
    host.await_status_is_healthy().unwrap();

    // Check the HostOS version again
    info!(
        logger,
        "Checking version via SSH on HostOS: '{}'",
        host.get_vm().expect("Unable to get HostOS VM.").ipv6
    );
    let new_version = check_hostos_version(&host);
    info!(logger, "Version found is: '{}'", new_version);

    assert!(new_version != original_version);
}
