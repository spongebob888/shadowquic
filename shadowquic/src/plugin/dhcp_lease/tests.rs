use super::*;
use crate::config::{DnsmasqLeaseCfg, OdhcpLeaseCfg};

fn record(snapshot: &Snapshot, ip: &str, now: u64) -> Option<parse::Lease> {
    snapshot.lookup(ip.parse().unwrap(), now).cloned()
}

#[test]
fn mixed_dnsmasq_and_duid_identity() {
    let snapshot = parse(
        "duid 00:03:00:01:ff:ff:ff:ff:ff:ff\n\
        0 02:00:00:00:00:98 192.0.2.1 Workstation *\n\
        200 T4294967295 2001:db8::1 Workstation 00:03:00:01:02:00:00:00:00:98\n\
        200 42 2001:db8::2 * *\n\
        200 00- 192.0.2.2 * *\n\
        200 06-aa:bb 192.0.2.3 node *\n\
        vendorclass 192.0.2.1 01:02\nagent-info 192.0.2.1 01:02\n# comment\n",
        false,
    )
    .unwrap();
    let v4 = record(&snapshot, "192.0.2.1", 200).unwrap();
    assert_eq!(v4.mac.as_deref(), Some("02:00:00:00:00:98"));
    let v6 = record(&snapshot, "2001:db8::1", 100).unwrap();
    assert_eq!(v6.mac, v4.mac);
    assert_eq!(v6.duid.as_deref(), Some("00:03:00:01:02:00:00:00:00:98"));
    assert_eq!(v6.iaid, Some(u32::MAX));
    assert_eq!(v6.host.as_deref(), Some(b"Workstation".as_slice()));
    let missing = record(&snapshot, "2001:db8::2", 100).unwrap();
    assert!(missing.host.is_none() && missing.duid.is_none() && missing.mac.is_none());
    assert_eq!(missing.iaid, Some(42));
    for ip in ["192.0.2.2", "192.0.2.3"] {
        assert!(record(&snapshot, ip, 100).unwrap().mac.is_none());
    }
    assert!(record(&snapshot, "2001:db8::1", 200).is_none());
}

#[test]
fn odhcp_mixed_records_addresses_prefixes_and_hostnames() {
    let snapshot = parse(
        r"# short comment
192.0.2.1 old-hosts-entry
# br-lan 2:0:0:0:0:98 ipv4 workstation -1 c0000201 32 192.0.2.1/32
# br-lan 000100012c444e2226f111bbf0d6 ffffffff CT103 200 b3b 128 2001:db8::1/128 fd00::1/128
# br-lan 00030001020000000098 a host\x2ename 200 1 128 2001:db8::2/128
# br-lan 00030001020000000098 b broken\x20bad\x20name 200 1 128 2001:db8::3/128
# br-lan 00030001020000000098 c - 200 1 64 2001:db8:1::/64
# br-lan 00030001020000000098 d - 200 1 128
# br-lan 00030001020000000098 e - 0 1 128 2001:db8::4/128
",
        true,
    )
    .unwrap();
    assert_eq!(
        record(&snapshot, "192.0.2.1", 1000).unwrap().mac.as_deref(),
        Some("02:00:00:00:00:98")
    );
    for ip in ["2001:db8::1", "fd00::1"] {
        let lease = record(&snapshot, ip, 100).unwrap();
        assert_eq!(lease.mac.as_deref(), Some("26:f1:11:bb:f0:d6"));
        assert_eq!(lease.iaid, Some(u32::MAX));
    }
    assert_eq!(
        record(&snapshot, "2001:db8::2", 100)
            .unwrap()
            .host
            .as_deref(),
        Some(b"host.name".as_slice())
    );
    assert!(
        record(&snapshot, "2001:db8::3", 100)
            .unwrap()
            .host
            .is_none()
    );
    assert!(record(&snapshot, "2001:db8:1::", 100).is_none());
    assert!(record(&snapshot, "2001:db8::4", 100).is_none());
}

#[test]
fn duids_without_ethernet_mac_keep_identity() {
    for duid in [
        "000200000009abcdef",
        "000400000000000000000000000000000000",
        "00030006020000000098",
        "000300010200",
        "00010001020000000098",
    ] {
        let snapshot = parse(
            &format!("# lan {duid} 1 host -1 1 128 2001:db8::1/128"),
            true,
        )
        .unwrap();
        let lease = record(&snapshot, "2001:db8::1", 100).unwrap();
        assert!(lease.mac.is_none(), "{duid}");
        assert!(lease.duid.is_some());
    }
}

#[test]
fn expiry_duplicates_and_wide_timestamps() {
    let snapshot = parse("0 02:00:00:00:00:01 192.0.2.1 first *\n200 02:00:00:00:00:02 192.0.2.1 * *\n5000000000 02:00:00:00:00:03 192.0.2.2 future *", false).unwrap();
    assert!(record(&snapshot, "192.0.2.1", 199).unwrap().host.is_none());
    assert_eq!(
        record(&snapshot, "192.0.2.1", 200).unwrap().host.as_deref(),
        Some(b"first".as_slice())
    );
    assert!(record(&snapshot, "192.0.2.2", 4999999999).is_some());
    assert!(record(&snapshot, "192.0.2.2", 5000000000).is_none());
}

#[test]
fn malformed_records_reject_snapshot() {
    for input in [
        "-1 02:00:00:00:00:01 192.0.2.1 host *",
        "200 4294967296 2001:db8::1 host *",
        "200 0xffffffff 2001:db8::1 host *",
        "200 1 2001:db8::1 host zz:00",
        "200 1 bad-ip host *",
        "200 02:00:00:00:00:01 192.0.2.1 host zz:00",
        "200 1 2001:db8::1",
    ] {
        assert!(parse(input, false).unwrap_err().contains("line 1"));
    }
    for input in [
        "# lan 0003 100000000 host 200 1 128 2001:db8::1/128",
        "# lan 0003 1 host -2 1 128 2001:db8::1/128",
        "# lan 0003 1 host 200 1 128 2001:db8::1/64",
        "# lan aa ipv4 host 200 1 32 2001:db8::1/32",
        "# lan aa ipv4",
    ] {
        assert!(parse(input, true).is_err(), "{input}");
    }
}

fn cfg(path: &std::path::Path) -> DhcpLeaseCfg {
    DhcpLeaseCfg::Dnsmasq(DnsmasqLeaseCfg {
        tag: "lan".into(),
        path: path.into(),
    })
}
fn host(lua: &Lua) -> Option<String> {
    lua.load("return find_dhcp_host_v4('lan', '192.0.2.1')")
        .eval()
        .unwrap()
}
fn wait_for(mut condition: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    while !condition() {
        assert!(
            std::time::Instant::now() < deadline,
            "lease update timed out"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn lua_contract_and_worker_cleanup() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("leases");
    fs::write(&path, "0 02:00:00:00:00:01 192.0.2.1 node *\n0 4294967295 2001:db8::1 node 00:03:00:01:02:00:00:00:00:01").unwrap();
    let store = LeaseStore::build(&[cfg(&path)]).unwrap();
    let weak = Arc::downgrade(&store);
    let lua = Lua::new();
    store.install(&lua).unwrap();
    lua.load(r#"
        assert(find_dhcp_host_v4('lan', '192.0.2.1') == 'node')
        assert(find_dhcp_mac_v4('lan', '192.0.2.1') == '02:00:00:00:00:01')
        assert(find_dhcp_host_v6('lan', '2001:db8::1') == 'node')
        assert(find_dhcp_mac_v6('lan', '2001:db8::1') == '02:00:00:00:00:01')
        assert(find_dhcp_iaid_v6('lan', '2001:db8::1') == 4294967295)
        assert(find_dhcp_duid_v6('lan', '2001:db8::1') == '00:03:00:01:02:00:00:00:00:01')
        assert(find_dhcp_host_v4('lan', '192.0.2.2') == nil)
        for _, args in ipairs({{'missing', '192.0.2.1'}, {'lan', 'bad'}, {'lan', '::1'}, {'lan', '192.0.2.1/32'}, {'lan', 123}, {123, '192.0.2.1'}}) do
            assert(not pcall(find_dhcp_host_v4, args[1], args[2]))
        end
        assert(not pcall(find_dhcp_host_v6, 'lan', 'fe80::1%lan'))
    "#).exec().unwrap();
    drop(store);
    assert!(weak.upgrade().is_some());
    drop(lua);
    assert!(weak.upgrade().is_none()); // Drop joins the worker.
}

#[test]
fn reload_replacement_failure_recreation_and_empty_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("leases");
    fs::write(&path, "0 02:00:00:00:00:01 192.0.2.1 old *").unwrap();
    let store = LeaseStore::build(&[cfg(&path)]).unwrap();
    let lua = Lua::new();
    store.install(&lua).unwrap();
    assert_eq!(host(&lua).as_deref(), Some("old"));
    fs::write(&path, "malformed").unwrap();
    store.sources["lan"].reload();
    assert_eq!(host(&lua).as_deref(), Some("old"));
    let replacement = dir.path().join("replacement");
    fs::write(&replacement, "0 02:00:00:00:00:02 192.0.2.1 new *").unwrap();
    fs::rename(replacement, &path).unwrap();
    wait_for(|| host(&lua).as_deref() == Some("new"));
    fs::remove_file(&path).unwrap();
    store.sources["lan"].reload();
    assert_eq!(host(&lua).as_deref(), Some("new"));
    fs::write(&path, "0 02:00:00:00:00:03 192.0.2.1 recreated *").unwrap();
    wait_for(|| host(&lua).as_deref() == Some("recreated"));
    fs::write(&path, "").unwrap();
    wait_for(|| host(&lua).is_none());
}

#[test]
fn missing_parent_recovers_by_reconciliation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("missing/leases");
    let store = LeaseStore::build(&[cfg(&path)]).unwrap();
    let lua = Lua::new();
    store.install(&lua).unwrap();
    assert!(host(&lua).is_none());
    fs::create_dir(path.parent().unwrap()).unwrap();
    fs::write(&path, "0 02:00:00:00:00:01 192.0.2.1 recovered *").unwrap();
    wait_for(|| host(&lua).as_deref() == Some("recovered"));
}

#[test]
fn odhcp_store_builds() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("leases");
    fs::write(
        &path,
        "# lan 02:00:00:00:00:01 ipv4 node -1 c0000201 32 192.0.2.1/32",
    )
    .unwrap();
    let store = LeaseStore::build(&[DhcpLeaseCfg::Odhcp(OdhcpLeaseCfg {
        tag: "lan".into(),
        path,
    })])
    .unwrap();
    let lua = Lua::new();
    store.install(&lua).unwrap();
    assert_eq!(host(&lua).as_deref(), Some("node"));
}

#[test]
fn failed_reload_retains_snapshot_but_not_expired_identity() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("leases");
    fs::write(&path, "200 02:00:00:00:00:01 192.0.2.1 old *").unwrap();
    let source = Source {
        tag: "lan".into(),
        path: path.clone(),
        odhcp: false,
        snapshot: ArcSwap::from_pointee(Snapshot::default()),
    };
    source.reload();
    fs::write(&path, "300 02:00:00:00:00:02 192.0.2.1 new *\nmalformed").unwrap();
    source.reload();
    let snapshot = source.snapshot.load();
    assert_eq!(
        record(&snapshot, "192.0.2.1", 199).unwrap().host.as_deref(),
        Some(b"old".as_slice())
    );
    assert!(record(&snapshot, "192.0.2.1", 200).is_none());
    fs::write(&path, "300 02:00:00:00:00:03 192.0.2.2 replacement *").unwrap();
    source.reload();
    let replacement = source.snapshot.load();
    assert!(record(&replacement, "192.0.2.1", 199).is_none());
    assert!(record(&replacement, "192.0.2.2", 199).is_some());
    // Readers that already acquired a snapshot retain a complete old version.
    assert!(record(&snapshot, "192.0.2.1", 199).is_some());
    assert!(record(&snapshot, "192.0.2.2", 199).is_none());
}
