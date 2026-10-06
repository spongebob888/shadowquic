use super::*;
use crate::{
    ProxyRequest,
    config::{Config, CountryDbCfg, GeositeDbCfg},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const YAML: &str = r#"lists:
  - name: test
    length: 5
    rules:
      - 'full:exact.example'
      - 'domain:tree.example:@ads'
      - 'keyword:needle'
      - 'regexp:^rx[0-9]+\.example$'
      - 'domain:second.example:@ads,@test'
  - name: other
    rules:
      - 'full:other.example'
"#;
fn config(dir: &Path, kind: RouterDBKind) -> RouterDatabaseCfg {
    match kind {
        RouterDBKind::Country => RouterDatabaseCfg::Country(CountryDbCfg {
            tag: "db".into(),
            url: "https://example.test/db".into(),
            path: dir.join("db.redb"),
        }),
        RouterDBKind::Geosite => RouterDatabaseCfg::Geosite(GeositeDbCfg {
            tag: "db".into(),
            url: "https://example.test/db".into(),
            path: dir.join("db.redb"),
        }),
    }
}
fn geosite(dir: &Path) -> (RouterDatabaseCfg, RedbDatabase) {
    let cfg = config(dir, RouterDBKind::Geosite);
    let source = dir.join("source.yml");
    std::fs::write(&source, YAML).unwrap();
    let db = RedbDatabase::import(&cfg, &source).unwrap();
    (cfg, db)
}
#[test]
fn geosite_indexed_and_sequential_rules_and_metadata_survive_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let (cfg, db) = geosite(dir.path());
    for domain in [
        "exact.example",
        "TREE.EXAMPLE.",
        "a.tree.example",
        "hasneedle.test",
        "rx123.example",
    ] {
        assert!(db.find_domain("TEST", domain).unwrap(), "{domain}");
    }
    for domain in [
        "sub.exact.example",
        "nottree.example",
        "other.example",
        "rxabc.example",
    ] {
        assert!(!db.find_domain("test", domain).unwrap(), "{domain}");
    }
    assert!(db.find_domain("test@ads", "tree.example").unwrap());
    assert!(!db.find_domain("test@test", "tree.example").unwrap());
    assert!(db.find_domain("test@test", "second.example").unwrap());
    assert!(!db.find_domain("missing", "exact.example").unwrap());
    let read = db.db.begin_read().unwrap();
    let meta = read.open_table(META).unwrap();
    assert_eq!(meta.get("schema").unwrap().unwrap().value(), "1");
    assert_eq!(
        meta.get("version").unwrap().unwrap().value(),
        env!("CARGO_PKG_VERSION")
    );
    assert_eq!(
        meta.get("sha256").unwrap().unwrap().value(),
        format!("{:x}", Sha256::digest(YAML.as_bytes()))
    );
    drop(meta);
    drop(read);
    drop(db);
    assert!(
        RedbDatabase::open(&cfg)
            .unwrap()
            .find_domain("test", "a.tree.example")
            .unwrap()
    );
    let mut changed = cfg;
    let RouterDatabaseCfg::Geosite(source) = &mut changed else {
        unreachable!()
    };
    source.url.push_str("/changed");
    let error = RedbDatabase::open(&changed).err().unwrap().to_string();
    assert!(error.contains("database \"db\""));
    assert!(error.contains(&format!("at {:?}", changed.path())));
    assert!(error.contains(
        "incompatible url: stored \"https://example.test/db\", expected \"https://example.test/db/changed\""
    ));
    assert!(error.contains(&format!("Remove the database file {:?}", changed.path())));
    assert!(error.contains("restart Shadowquic to download and rebuild it"));
}
#[test]
fn database_opens_with_missing_or_different_application_version() {
    let dir = tempfile::tempdir().unwrap();
    let (cfg, db) = geosite(dir.path());
    drop(db);
    for version in [Some("0.0.0"), None] {
        let db = Database::open(cfg.path()).unwrap();
        let write = db.begin_write().unwrap();
        {
            let mut meta = write.open_table(META).unwrap();
            match version {
                Some(version) => {
                    meta.insert("version", version).unwrap();
                }
                None => {
                    meta.remove("version").unwrap();
                }
            }
        }
        write.commit().unwrap();
        drop(db);
        assert!(
            RedbDatabase::open(&cfg)
                .unwrap()
                .find_domain("test", "exact.example")
                .unwrap()
        );
    }
}

#[test]
fn failed_import_never_publishes_or_replaces_database() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path(), RouterDBKind::Geosite);
    let source = dir.path().join("bad.yml");
    for data in [
        "not yaml",
        "lists: []",
        "lists: [{name: test, rules: ['regexp:(']}]",
    ] {
        std::fs::write(&source, data).unwrap();
        assert!(RedbDatabase::import(&cfg, &source).is_err());
        assert!(!cfg.path().exists());
    }
    let (_, db) = geosite(dir.path());
    drop(db);
    std::fs::write(&source, "lists: [{name: test, rules: ['full:new.test']}]").unwrap();
    assert!(RedbDatabase::import(&cfg, &source).is_err());
    assert!(
        RedbDatabase::open(&cfg)
            .unwrap()
            .find_domain("test", "exact.example")
            .unwrap()
    );
}
#[test]
fn country_import_matches_mmdb_for_ipv4_and_ipv6() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path(), RouterDBKind::Country);
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/router-database/GeoIP2-Country-Test.mmdb");
    let db = RedbDatabase::import(&cfg, &source).unwrap();
    drop(db);
    let db = RedbDatabase::open(&cfg).unwrap();
    let read = db.db.begin_read().unwrap();
    assert_eq!(
        read.open_table(META)
            .unwrap()
            .get("schema")
            .unwrap()
            .unwrap()
            .value(),
        "2"
    );
    assert!(matches!(
        read.open_table(RULES),
        Err(redb::TableError::TableDoesNotExist(_))
    ));
    let reader = maxminddb::Reader::open_readfile(&source).unwrap();
    let mut families = [false; 2];
    for result in reader.networks(Default::default()).unwrap() {
        let result = result.unwrap();
        let record = result
            .decode::<maxminddb::geoip2::Country>()
            .unwrap()
            .unwrap();
        let Some(code) = record.country.iso_code else {
            continue;
        };
        let network = result.network().unwrap();
        let ip = network.ip();
        families[usize::from(ip.is_ipv6())] = true;
        assert!(db.find_ip(code, ip).unwrap(), "{network} {code}");
        assert!(db.find_ip(code, network.broadcast()).unwrap());
        let v4_name = format!("country_v4_{}", code.to_ascii_lowercase());
        let v6_name = format!("country_v6_{}", code.to_ascii_lowercase());
        // Both tables exist even if this country only has one address family.
        let v4 = read
            .open_table(TableDefinition::<u32, u32>::new(&v4_name))
            .unwrap();
        let v6 = read
            .open_table(TableDefinition::<u128, u128>::new(&v6_name))
            .unwrap();
        match (network.network(), network.broadcast()) {
            (IpAddr::V4(start), IpAddr::V4(end)) => {
                let (_, stored_end) = v4
                    .range(..=u32::from(start))
                    .unwrap()
                    .next_back()
                    .unwrap()
                    .unwrap();
                assert!(stored_end.value() >= u32::from(end));
            }
            (IpAddr::V6(start), IpAddr::V6(end)) => {
                let (_, stored_end) = v6
                    .range(..=u128::from(start))
                    .unwrap()
                    .next_back()
                    .unwrap()
                    .unwrap();
                assert!(stored_end.value() >= u128::from(end));
            }
            _ => unreachable!(),
        }
        assert!(!db.find_ip("missing", ip).unwrap());
        if let Some(name) = record.country.names.english {
            assert!(!db.find_ip(name, ip).unwrap());
            for family in [4, 6] {
                let name = country_table_name(name, family);
                assert!(
                    !read
                        .list_tables()
                        .unwrap()
                        .any(|table| table.name() == name)
                );
            }
        }
    }
    assert_eq!(families, [true, true]);
    assert!(!db.find_ip("US", "127.0.0.1".parse().unwrap()).unwrap());
    for handle in read.list_tables().unwrap() {
        if handle.name().starts_with("country_v4_") {
            let table = read.open_table(CountryV4Table::new(handle.name())).unwrap();
            let mut previous: Option<u32> = None;
            for entry in table.iter().unwrap() {
                let (start, end) = entry.unwrap();
                if let Some(previous) = previous {
                    assert!(previous.checked_add(1).unwrap() < start.value());
                }
                previous = Some(end.value());
            }
        } else if handle.name().starts_with("country_v6_") {
            let table = read.open_table(CountryV6Table::new(handle.name())).unwrap();
            let mut previous: Option<u128> = None;
            for entry in table.iter().unwrap() {
                let (start, end) = entry.unwrap();
                if let Some(previous) = previous {
                    assert!(previous.checked_add(1).unwrap() < start.value());
                }
                previous = Some(end.value());
            }
        }
    }
}

#[test]
fn country_import_merges_adjacent_ranges_in_either_order_without_crossing_gaps() {
    fn check<T>(maximum: T)
    where
        T: for<'a> redb::Key<SelfType<'a> = T>
            + Copy
            + Ord
            + Into<u128>
            + From<u8>
            + std::fmt::Debug
            + 'static,
    {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::create(dir.path().join("merge.redb")).unwrap();
        let write = db.begin_write().unwrap();
        let mut table = write
            .open_table(TableDefinition::<T, T>::new("ranges"))
            .unwrap();
        // Exercise a predecessor merge, successor merge, and a bridge joining both.
        for (start, end) in [
            (10, 19),
            (20, 29),
            (5, 9),
            (40, 49),
            (30, 39),
            (51, 60),
            (0, 0),
        ] {
            insert_country_range(&mut table, T::from(start), T::from(end)).unwrap();
        }
        insert_country_range(&mut table, maximum, maximum).unwrap();
        let ranges: Vec<_> = table
            .iter()
            .unwrap()
            .map(|entry| {
                let (start, end) = entry.unwrap();
                (start.value(), end.value())
            })
            .collect();
        assert_eq!(
            ranges,
            vec![
                (T::from(0), T::from(0)),
                (T::from(5), T::from(49)),
                (T::from(51), T::from(60)),
                (maximum, maximum)
            ]
        );
    }
    check(u32::MAX);
    check(u128::MAX);
}

#[test]
fn country_ranges_match_boundaries_gaps_and_separate_families() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::create(dir.path().join("ranges.redb")).unwrap();
    let write = db.begin_write().unwrap();
    write
        .open_table(CountryV4Table::new("country_v4_empty"))
        .unwrap();
    write
        .open_table(CountryV6Table::new("country_v6_empty"))
        .unwrap();
    {
        let mut table = write
            .open_table(CountryV4Table::new("country_v4_test"))
            .unwrap();
        // Include values crossing a byte boundary to exercise numeric ordering.
        for (start, end) in [
            (0, 0),
            (10, 20),
            (255, 256),
            (0x01020304, 0x01020304),
            (u32::MAX, u32::MAX),
        ] {
            table.insert(start, end).unwrap();
        }
        let mut table = write
            .open_table(CountryV6Table::new("country_v6_test"))
            .unwrap();
        for (start, end) in [
            (10, 20),
            (
                0x20010db81234567890abcdef01234567,
                0x20010db81234567890abcdef01234567,
            ),
            (u128::MAX, u128::MAX),
        ] {
            table.insert(start, end).unwrap();
        }
    }
    write.commit().unwrap();
    let db = RedbDatabase {
        db,
        kind: RouterDBKind::Country,
    };
    for (ip, expected) in [
        ("0.0.0.0", true),
        ("0.0.0.1", false),
        ("0.0.0.9", false),
        ("0.0.0.10", true),
        ("0.0.0.15", true),
        ("0.0.0.20", true),
        ("0.0.0.21", false),
        ("0.0.0.255", true),
        ("0.0.1.0", true),
        ("0.0.1.1", false),
        ("1.2.3.4", true),
        ("4.3.2.1", false),
        ("255.255.255.255", true),
        ("::", false),
        ("::9", false),
        ("::a", true),
        ("::f", true),
        ("::14", true),
        ("::15", false),
        ("::ffff:ffff", false),
        ("::ffff:0.0.0.10", false),
        ("2001:db8:1234:5678:90ab:cdef:123:4567", true),
        ("6745:2301:efcd:ab90:7856:3412:b80d:120", false),
        ("ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff", true),
    ] {
        let ip = ip.parse().unwrap();
        assert_eq!(db.find_ip("TEST", ip).unwrap(), expected, "{ip}");
        assert!(!db.find_ip("missing", ip).unwrap());
        assert!(!db.find_ip("empty", ip).unwrap());
    }
}

#[test]
fn incompatible_schema_is_rejected_for_each_database_type() {
    for (kind, stored, expected) in [
        (RouterDBKind::Country, "1", "2"),
        (RouterDBKind::Geosite, "2", "1"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path(), kind);
        let db = Database::create(cfg.path()).unwrap();
        let write = db.begin_write().unwrap();
        {
            let mut meta = write.open_table(META).unwrap();
            meta.insert("schema", stored).unwrap();
            write.open_table(RULES).unwrap();
        }
        write.commit().unwrap();
        drop(db);
        let error = RedbDatabase::open(&cfg).err().unwrap().to_string();
        assert!(
            error.contains(&format!(
                "incompatible schema: stored {stored:?}, expected {expected:?}"
            )),
            "{error}"
        );
        assert!(error.contains("Remove the database file"));
    }
}
#[test]
fn lua_helpers_report_unavailable_unknown_and_wrong_family() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path(), RouterDBKind::Country);
    let manager = Databases::build(&[cfg], &mut HashMap::new()).unwrap();
    let lua = mlua::Lua::new();
    manager.install(&lua).unwrap();
    for (expr, error) in [
        (
            "find_ip_v4('unknown', 'US', '1.1.1.1')",
            "unknown router database",
        ),
        ("find_ip_v4('db', 'US', '1.1.1.1')", "download pending"),
        (
            "find_domain('db', 'test', 'example.test')",
            "does not support",
        ),
    ] {
        assert!(
            lua.load(expr)
                .eval::<bool>()
                .unwrap_err()
                .to_string()
                .contains(error)
        );
    }
    let other = tempfile::tempdir().unwrap();
    let (cfg, db) = geosite(other.path());
    drop(db);
    let mut inbounds = HashMap::new();
    let manager = Databases::build(&[cfg], &mut inbounds).unwrap();
    assert!(inbounds.is_empty());
    manager.install(&lua).unwrap();
    assert!(
        lua.load("find_domain('db', 'test', 'tree.example')")
            .eval::<bool>()
            .unwrap()
    );
}
#[test]
fn database_config_rejects_tag_collisions_and_bad_urls() {
    let example: Config = serde_saphyr::from_str(include_str!(
        "../../../config_examples/router-database.yaml"
    ))
    .unwrap();
    example.validate().unwrap();
    let base = "inbounds: [{type: socks, tag: in, bind-addr: '127.0.0.1:0'}]\noutbounds: [{type: direct, tag: out}]\n";
    for tag in ["in", "out", "default-system", "", "db"] {
        let count = if tag == "db" { 2 } else { 1 };
        let mut config: Config = serde_saphyr::from_str(base).unwrap();
        config.router.database = (0..count)
            .map(|i| {
                RouterDatabaseCfg::Geosite(GeositeDbCfg {
                    tag: tag.into(),
                    url: "https://example.test/db".into(),
                    path: format!("db{i}.redb").into(),
                })
            })
            .collect();
        assert!(config.validate().is_err(), "{tag}");
    }
    for url in ["file:///tmp/db", "https://user:pass@example.test/db", "bad"] {
        assert!(validate_url(url).is_err());
    }
}

#[tokio::test]
async fn download_uses_tagged_inbound_for_redirects_and_publishes_database() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path(), RouterDBKind::Geosite);
    // Use HTTP locally while still asserting that no direct connection is made:
    // the test serves all responses over the emitted proxy sessions.
    let mut cfg = cfg;
    let RouterDatabaseCfg::Geosite(source) = &mut cfg else {
        unreachable!()
    };
    source.url = "http://download.test/start".into();
    let mut inbounds = HashMap::new();
    let manager = Databases::build(std::slice::from_ref(&cfg), &mut inbounds).unwrap();
    let mut inbound = inbounds.remove("db").unwrap();
    inbound.init().await.unwrap();
    for redirect in [true, false] {
        let req = tokio::time::timeout(std::time::Duration::from_secs(5), inbound.accept())
            .await
            .unwrap()
            .unwrap();
        let ProxyRequest::Tcp(mut session) = req else {
            panic!("expected TCP")
        };
        assert_eq!(session.user_context.inbound_tag, "db");
        assert_eq!(
            session.dst.to_string(),
            if redirect {
                "download.test:80"
            } else {
                "redirect.test:80"
            }
        );
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            request.push(session.stream.read_u8().await.unwrap());
        }
        let response = if redirect {
            "HTTP/1.1 302 Found\r\nLocation: http://redirect.test/db\r\nContent-Length: 0\r\n\r\n"
                .to_owned()
        } else {
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{YAML}",
                YAML.len()
            )
        };
        session.stream.write_all(response.as_bytes()).await.unwrap();
        session.stream.shutdown().await.unwrap();
    }
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if manager.slots["db"].value.read().unwrap().is_ok() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(cfg.path().exists());
    let lua = mlua::Lua::new();
    manager.install(&lua).unwrap();
    assert!(
        lua.load("find_domain('db', 'test', 'tree.example')")
            .eval::<bool>()
            .unwrap()
    );
    inbound.shutdown().await.unwrap();
}

#[test]
#[ignore = "requires ROUTER_GEOSITE_SOURCE pointing to the published YAML export"]
fn published_geosite_import() {
    let source = std::env::var_os("ROUTER_GEOSITE_SOURCE").expect("set ROUTER_GEOSITE_SOURCE");
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path(), RouterDBKind::Geosite);
    let db = RedbDatabase::import(&cfg, Path::new(&source)).unwrap();
    assert!(db.find_domain("google", "www.google.com").unwrap());
}

#[tokio::test]
async fn manager_routes_database_download_to_lua_selected_outbound() {
    struct ServeDatabase;
    #[async_trait::async_trait]
    impl crate::Outbound for ServeDatabase {
        async fn handle(&self, req: ProxyRequest) -> std::result::Result<(), SError> {
            let ProxyRequest::Tcp(mut session) = req else {
                panic!("expected TCP")
            };
            assert_eq!(session.user_context.inbound_tag, "db");
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(session.stream.read_u8().await?);
            }
            session
                .stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{YAML}",
                        YAML.len()
                    )
                    .as_bytes(),
                )
                .await?;
            session.stream.shutdown().await?;
            Ok(())
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let mut cfg: Config = serde_saphyr::from_str("inbounds: [{type: socks, tag: in, bind-addr: '127.0.0.1:0'}]\noutbounds: [{type: drop, tag: wrong}, {type: direct, tag: selected}]\n").unwrap();
    let mut db = config(dir.path(), RouterDBKind::Geosite);
    let RouterDatabaseCfg::Geosite(source) = &mut db else {
        unreachable!()
    };
    source.url = "http://database.test/source".into();
    cfg.router.database.push(db.clone());
    cfg.router.src = Some("return function(ctx) if ctx.inbound_tag == 'db' then return 'selected' end return 'wrong' end".into());
    let mut manager = cfg.build_manager().await.unwrap();
    assert!(manager.inbounds.contains_key("db"));
    manager.inbounds.remove("in");
    manager
        .outbounds
        .insert("selected".into(), Arc::new(ServeDatabase));
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        manager.run_until(async {
            while !db.path().exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        RedbDatabase::open(&db)
            .unwrap()
            .find_domain("test", "tree.example")
            .unwrap()
    );
}

#[tokio::test]
async fn failed_http_download_reports_error_without_publishing() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(dir.path(), RouterDBKind::Geosite);
    let RouterDatabaseCfg::Geosite(source) = &mut cfg else {
        unreachable!()
    };
    source.url = "http://database.test/missing".into();
    let mut inbounds = HashMap::new();
    let databases = Databases::build(std::slice::from_ref(&cfg), &mut inbounds).unwrap();
    let mut inbound = inbounds.remove("db").unwrap();
    inbound.init().await.unwrap();
    let ProxyRequest::Tcp(mut session) = inbound.accept().await.unwrap() else {
        panic!("expected TCP")
    };
    let mut request = Vec::new();
    while !request.ends_with(b"\r\n\r\n") {
        request.push(session.stream.read_u8().await.unwrap());
    }
    session
        .stream
        .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if databases.slots["db"]
                .value
                .read()
                .unwrap()
                .as_ref()
                .err()
                .unwrap()
                .contains("404")
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(!cfg.path().exists());
    inbound.shutdown().await.unwrap();
}
