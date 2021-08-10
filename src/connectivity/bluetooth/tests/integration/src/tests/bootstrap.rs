// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    anyhow::{format_err, Context as _, Error},
    bt_test_harness::{
        access::{AccessHarness, AccessState},
        bootstrap::BootstrapHarness,
    },
    fidl_fuchsia_bluetooth_sys as sys,
    fuchsia_bluetooth::expectation::{
        asynchronous::{ExpectableExt, ExpectableStateExt},
        Predicate as P,
    },
    fuchsia_bluetooth::types::{
        Address, BondingData, HostData, Identity, LeBondData, OneOrBoth, PeerId,
    },
    std::collections::HashSet,
    test_harness::run_suite,
};

use crate::tests::timeout_duration;

/// An example identity for an Hci Emulator backed host
fn example_emulator_identity() -> Identity {
    // Hci Emulators currently default to address 0
    let emulator_address = Address::Public([0, 0, 0, 0, 0, 0]);
    let peer_address = Address::Random([1, 0, 0, 0, 0, 0]);

    let host_data = HostData {
        irk: Some(sys::Key { value: [16, 15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1] }),
    };
    let csrk = sys::PeerKey {
        data: sys::Key { value: [0; 16] },
        security: sys::SecurityProperties {
            authenticated: false,
            secure_connections: true,
            encryption_key_size: 0,
        },
    };
    let le_data = LeBondData {
        connection_parameters: None,
        services: vec![],
        peer_ltk: None,
        local_ltk: None,
        irk: None,
        csrk: Some(csrk),
    };
    let bonds = vec![BondingData {
        identifier: PeerId(42),
        address: peer_address,
        local_address: emulator_address,
        name: None,
        data: OneOrBoth::Left(le_data),
    }];
    Identity { host: host_data, bonds }
}

async fn test_add_and_commit_identities(
    (bootstrap, access): (BootstrapHarness, AccessHarness),
) -> Result<(), Error> {
    let bootstrap = bootstrap.aux().clone();

    let access_proxy = access.aux().clone();
    // We avoid using an assert here because the unwinding causes the ActivatedFakeHost to
    // panic during the unwind which then just aborts. This provides a much clearer and more
    // graceful failure case
    let (initial_devices, _) = access_proxy
        .watch_peers()
        .await
        .ok()
        .ok_or(format_err!("Missing response to WatchPeers()"))?;
    if initial_devices != vec![] {
        return Err(format_err!(
            "Failed: Initial devices not empty! Expected empty, found: {:?}",
            initial_devices
        ));
    }

    let identity = example_emulator_identity();

    let expected_peers: HashSet<PeerId> = identity.bonds.iter().map(|b| b.identifier).collect();

    let identities: Vec<sys::Identity> = vec![identity.into()];
    bootstrap.add_identities(&mut identities.into_iter()).context("Error adding identities")?;
    bootstrap.commit().await?.map_err(|e| format_err!("Error committing bonds: {:?}", e))?;

    let pred = P::<AccessState>::predicate(
        move |access| expected_peers == access.peers.keys().cloned().collect(),
        "known device identifiers == expected device identifiers",
    );
    access.when_satisfied(pred, timeout_duration()).await?;

    Ok(())
}

pub fn run_all() -> Result<(), Error> {
    run_suite!("sys.Bootstrap", [test_add_and_commit_identities])
}
