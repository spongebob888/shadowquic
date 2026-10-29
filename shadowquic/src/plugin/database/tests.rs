use super::*;
use crate::{
    ProxyRequest,
    config::{Config, CountryDbCfg, GeositeDbCfg},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const RULES: TableDefinition<&str, ()> = TableDefinition::new("rules");

const YAML: &str = r#"lists:
  - name: test
    length: 5
    rules:
      - 'full:exact.example'
      - 'domain:tree.example:@ads'
      - 'keyword:needle'
      - 'regexp:^rx[0-9]+\.example$'
      - 'domain:second.example:@ads'
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
            list_include: Vec::new(),
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

fn legacy_geosite(cfg: &RouterDatabaseCfg, version: &str) {
    let db = Database::create(cfg.path()).unwrap();
    let write = db.begin_write().unwrap();
    {
        let mut meta = write.open_table(META).unwrap();
        for (key, value) in [
            ("schema", version),
            ("type", "geosite"),
            ("url", cfg.url()),
            ("sha256", "legacy"),
            ("legacy-only", "remove me"),
        ] {
            meta.insert(key, value).unwrap();
        }
        write.open_table(RULES).unwrap().insert("old", ()).unwrap();
        write
            .open_table(TableDefinition::<(u8, &[u8]), u8>::new("geosite_test"))
            .unwrap()
            .insert((1, b"old.example".as_slice()), 3)
            .unwrap();
        write
            .open_multimap_table(redb::MultimapTableDefinition::<u8, u8>::new("old_multimap"))
            .unwrap()
            .insert(1, 2)
            .unwrap();
    }
    write.commit().unwrap();
}

fn country_fixture() -> &'static [u8] {
    include_bytes!("../../../tests/fixtures/router-database/GeoIP2-Country-Test.mmdb")
}

fn country_config(dir: &Path, filename: &str) -> RouterDatabaseCfg {
    RouterDatabaseCfg::Country(CountryDbCfg {
        tag: "db".into(),
        url: "http://download.test/country".into(),
        path: dir.join(filename),
    })
}

#[test]
fn mmap_country_matches_in_memory_reader_at_all_fixture_range_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = country_config(dir.path(), "country.mmdb");
    std::fs::write(cfg.path(), country_fixture()).unwrap();
    let db = open_database(&cfg).unwrap();
    let reader = maxminddb::Reader::from_source(country_fixture()).unwrap();
    for entry in reader
        .networks(maxminddb::WithinOptions::default().include_aliased_networks())
        .unwrap()
    {
        let network = entry.unwrap().network().unwrap();
        for ip in [network.network(), network.broadcast()] {
            let code = reader
                .lookup(ip)
                .unwrap()
                .decode::<maxminddb::geoip2::Country>()
                .unwrap()
                .and_then(|record| record.country.iso_code);
            let lowercase = code.unwrap_or("missing").to_ascii_lowercase();
            for list in [lowercase.as_str(), "GB", "US", "CN", "JP", "missing"] {
                assert_eq!(
                    db.find_ip(list, ip).unwrap(),
                    code.is_some_and(|code| code.eq_ignore_ascii_case(list)),
                    "{ip} {list}"
                );
            }
        }
    }
}

#[test]
fn country_backends_publish_reopen_and_work_through_lua() {
    for filename in ["country.mmdb", "country.MMDB", "country.redb", "legacy"] {
        let dir = tempfile::tempdir().unwrap();
        let cfg = country_config(dir.path(), filename);
        let source = dir.path().join("source");
        std::fs::write(&source, country_fixture()).unwrap();
        drop(import_database(&cfg, &source).unwrap());
        if cfg.uses_mmdb() {
            assert_eq!(std::fs::read(cfg.path()).unwrap(), country_fixture());
        } else {
            // Existing schema-2 redb files remain readable by the original backend.
            assert!(RedbDatabase::open(&cfg).is_ok());
        }
        let mut inbounds = HashMap::new();
        let manager = Databases::build(std::slice::from_ref(&cfg), &mut inbounds).unwrap();
        assert!(inbounds.is_empty());
        let lua = mlua::Lua::new();
        manager.install(&lua).unwrap();
        for (expr, expected) in [
            ("find_ip_v4('db', 'gb', '81.2.69.160')", true),
            ("find_ip_v4('db', 'US', '81.2.69.160')", false),
            ("find_ip_v4('db', 'missing', '81.2.69.160')", false),
            ("find_ip_v6('db', 'JP', '2001:218::')", true),
            ("find_ip_v6('db', 'GB', '::1')", false),
        ] {
            assert_eq!(
                lua.load(expr).eval::<bool>().unwrap(),
                expected,
                "{filename}: {expr}"
            );
        }
        if cfg.uses_mmdb() {
            for address in [
                "::ffff:81.2.69.160",
                "2002:5102:45a0:2547:dcdc:9e8f:e890:d540",
                "2001:0:5102:45a0:dcdc:9e8f:e890:d540",
            ] {
                assert!(
                    lua.load(format!("find_ip_v6('db', 'GB', '{address}')"))
                        .eval::<bool>()
                        .unwrap()
                );
            }
        }
        for expr in [
            "find_domain('db', 'GB', 'example.test')",
            "find_ip_v4('db', 'GB', '::1')",
            "find_ip_v6('db', 'GB', 'invalid')",
        ] {
            assert!(lua.load(expr).eval::<bool>().is_err());
        }
    }
}

#[test]
fn invalid_mmdb_is_not_published_and_existing_files_are_not_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = country_config(dir.path(), "country.mmdb");
    let source = dir.path().join("source");
    std::fs::write(&source, b"not an MMDB").unwrap();
    assert!(import_database(&cfg, &source).is_err());
    assert!(!cfg.path().exists());
    std::fs::write(&source, country_fixture()).unwrap();
    std::fs::write(cfg.path(), b"existing invalid database").unwrap();
    assert!(Databases::build(std::slice::from_ref(&cfg), &mut HashMap::new()).is_err());
    assert!(import_database(&cfg, &source).is_err());
    assert_eq!(
        std::fs::read(cfg.path()).unwrap(),
        b"existing invalid database"
    );
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
}

#[tokio::test]
async fn country_mmdb_download_publishes_original_bytes_and_reports_invalid_data() {
    for valid in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let cfg = country_config(dir.path(), "nested/country.mmdb");
        let mut inbounds = HashMap::new();
        let manager = Databases::build(std::slice::from_ref(&cfg), &mut inbounds).unwrap();
        let lua = mlua::Lua::new();
        manager.install(&lua).unwrap();
        assert!(
            lua.load("find_ip_v4('db', 'GB', '81.2.69.160')")
                .eval::<bool>()
                .unwrap_err()
                .to_string()
                .contains("download pending")
        );
        let mut inbound = inbounds.remove("db").unwrap();
        inbound.init().await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let ProxyRequest::Tcp(mut session) = inbound.accept().await.unwrap() else {
                panic!("expected TCP")
            };
            assert_eq!(session.user_context.inbound_tag, "db");
            assert_eq!(session.dst.to_string(), "download.test:80");
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(session.stream.read_u8().await.unwrap());
            }
            let body = if valid {
                country_fixture()
            } else {
                b"invalid MMDB"
            };
            session
                .stream
                .write_all(
                    format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).as_bytes(),
                )
                .await
                .unwrap();
            session.stream.write_all(body).await.unwrap();
            session.stream.shutdown().await.unwrap();
            assert!(matches!(inbound.accept().await, Err(SError::InboundClosed)));
        })
        .await
        .unwrap();
        if valid {
            assert_eq!(std::fs::read(cfg.path()).unwrap(), country_fixture());
            assert!(
                lua.load("find_ip_v6('db', 'GB', '2002:5102:45a0::1')")
                    .eval::<bool>()
                    .unwrap()
            );
            assert!(
                open_database(&cfg)
                    .unwrap()
                    .find_ip("GB", "81.2.69.160".parse().unwrap())
                    .unwrap()
            );
        } else {
            assert!(!cfg.path().exists());
            assert!(
                lua.load("find_ip_v4('db', 'GB', '81.2.69.160')")
                    .eval::<bool>()
                    .is_err()
            );
        }
        inbound.shutdown().await.unwrap();
    }
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
    assert!(!db.find_domain("test@test", "second.example").unwrap());
    assert!(!db.find_domain("missing", "exact.example").unwrap());
    let read = db.db.begin_read().unwrap();
    let meta = read.open_table(META).unwrap();
    assert_eq!(meta.get("schema").unwrap().unwrap().value(), "3");
    assert_eq!(
        meta.get("version").unwrap().unwrap().value(),
        env!("CARGO_PKG_VERSION")
    );
    assert_eq!(
        meta.get("sha256").unwrap().unwrap().value(),
        "00dc0abfd79ca20764d98754688fc9d59e922dedfaf81a17c983c90b3f74c581"
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
fn geosite_schema_three_tables_preserve_attribute_masks_and_isolate_lists() {
    use super::geosite::GeositeTable;
    use redb::ReadableTableMetadata;
    use regex_automata::dfa::Automaton;

    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path(), RouterDBKind::Geosite);
    let source = dir.path().join("source.yml");
    std::fs::write(
        &source,
        r#"lists:
  - name: Test
    rules:
      - 'full:EXACT.example:@ads,@telemetry'
      - 'full:exact.example:@cn'
      - 'domain:tree.example:@cn,@telemetry'
      - 'keyword:NEEDLE:@!cn,@telemetry'
      - 'regexp:^rx\d+\.example$:@ads,@cn,@telemetry'
      - 'regexp:^\bfallback\.example$:@!cn,@telemetry'
      - 'full:plain.example'
      - 'full:unknown.example:@other,@new-attribute'
      - 'full:mixed.example:@other,@!cn'
      - 'full:merged.example:@telemetry'
  - name: TEST
    rules:
      - 'full:merged.example:@ADS'
  - name: empty
    rules: []
  - name: metadata
    rules:
      - 'full:other.example'
"#,
    )
    .unwrap();
    drop(RedbDatabase::import(&cfg, &source).unwrap());
    let db = RedbDatabase::open(&cfg).unwrap();
    for (domain, attrs) in [
        ("EXACT.example.", [true, false, true, true]),
        ("a.b.tree.example", [false, false, true, true]),
        ("hasneedle.example", [false, true, false, true]),
        ("rx123.example", [true, false, true, true]),
        ("fallback.example", [false, true, false, true]),
        ("plain.example", [false, false, false, false]),
        ("unknown.example", [false, false, false, false]),
        ("mixed.example", [false, true, false, false]),
        ("merged.example", [true, false, false, true]),
    ] {
        assert!(db.find_domain("TEST", domain).unwrap(), "{domain}");
        for (attr, expected) in ["ADS", "!CN", "CN", "TELEMETRY"].into_iter().zip(attrs) {
            assert_eq!(
                db.find_domain(&format!("TeSt@{attr}"), domain).unwrap(),
                expected,
                "{domain}@{attr}"
            );
        }
        for list in ["empty", "missing", "metadata", "test@other", "test@"] {
            assert!(!db.find_domain(list, domain).unwrap(), "{list}: {domain}");
        }
    }
    for domain in [
        "sub.exact.example",
        "nottree.example",
        "rxabc.example",
        "other.example",
    ] {
        assert!(!db.find_domain("test", domain).unwrap(), "{domain}");
    }
    assert!(db.find_domain("metadata", "other.example").unwrap());
    let read = db.db.begin_read().unwrap();
    let names: Vec<_> = read
        .list_tables()
        .unwrap()
        .map(|table| table.name().to_owned())
        .collect();
    assert_eq!(
        names,
        [
            "geosite_empty",
            "geosite_metadata",
            "geosite_test",
            "metadata"
        ]
    );
    let table = read.open_table(GeositeTable::new("geosite_test")).unwrap();
    assert_eq!(table.len().unwrap(), 9);
    assert_eq!(
        table
            .get((0u8, b"exact.example".as_slice()))
            .unwrap()
            .unwrap()
            .value(),
        1 | 4 | 8
    );
    assert_eq!(
        table
            .get((0u8, b"unknown.example".as_slice()))
            .unwrap()
            .unwrap()
            .value(),
        0
    );
    let (regex_key, regex_attribute) = table
        .range((4u8, &b""[..])..(5u8, &b""[..]))
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    assert_eq!(regex_attribute.value(), 1 | 4 | 8);
    assert_eq!(
        table
            .get((2u8, br"^\bfallback\.example$".as_slice()))
            .unwrap()
            .unwrap()
            .value(),
        2 | 8
    );
    let (_, dfa_bytes) = regex_key.value();
    assert!(dfa_bytes.len() > 16);
    let dfa = super::geosite::deserialize_dfa(dfa_bytes).unwrap();
    assert!(
        dfa.try_search_fwd(&regex_automata::Input::new("rx123.example"))
            .unwrap()
            .is_some(),
        "compiled regex DFA must be stored under the CompiledRegex key type"
    );
}

#[test]
fn geosite_unknown_attributes_warn_once_and_keep_known_bits() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path(), RouterDBKind::Geosite);
    let source = dir.path().join("source.yml");
    std::fs::write(
        &source,
        "lists: [{name: test, rules: ['full:unknown.example:@new-attr', 'full:mixed.example:@NEW-ATTR,@telemetry']}]",
    )
    .unwrap();
    let logs = tempfile::NamedTempFile::new().unwrap();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .without_time()
        .with_ansi(false)
        .with_writer(Arc::new(logs.reopen().unwrap()))
        .finish();
    // Keep two dispatchers alive so tracing registers callsites against both,
    // rather than caching the default of a parallel test that has no subscriber.
    let _other_dispatch = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
    tracing::subscriber::with_default(subscriber, || {
        // Register the same warning callsite from an unsubscribed thread first,
        // reproducing the order that previously lost the captured warning in CI.
        let other_source = source.clone();
        std::thread::spawn(move || {
            let dir = tempfile::tempdir().unwrap();
            RedbDatabase::import(&config(dir.path(), RouterDBKind::Geosite), &other_source)
                .unwrap();
        })
        .join()
        .unwrap();
        let db = RedbDatabase::import(&cfg, &source).unwrap();
        assert!(db.find_domain("test", "unknown.example").unwrap());
        assert!(!db.find_domain("test@telemetry", "unknown.example").unwrap());
        assert!(db.find_domain("test@telemetry", "mixed.example").unwrap());
        assert!(!db.find_domain("test@new-attr", "mixed.example").unwrap());
    });
    let logs = std::fs::read_to_string(logs.path()).unwrap();
    assert!(logs.contains("new-attr"), "{logs}");
    assert_eq!(
        logs.matches("ignoring unknown geosite attribute").count(),
        1,
        "{logs}"
    );
}

#[test]
fn geosite_legacy_schemas_recreate_all_tables_with_u32_attribute_masks() {
    for version in ["0", "1", "2"] {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path(), RouterDBKind::Geosite);
        legacy_geosite(&cfg, version);
        let error = RedbDatabase::open(&cfg).err().unwrap();
        assert!(error.is::<GeositeRebuildRequired>());
        let source = dir.path().join("source.yml");
        let yaml = YAML.replace("tree.example:@ads", "tree.example:@ads,@telemetry");
        std::fs::write(&source, &yaml).unwrap();
        let db = RedbDatabase::import(&cfg, &source).unwrap();
        assert!(db.find_domain("test@ads", "tree.example").unwrap());
        assert!(db.find_domain("test@telemetry", "tree.example").unwrap());
        assert!(!db.find_domain("test", "old.example").unwrap());
        let read = db.db.begin_read().unwrap();
        assert_eq!(
            read.list_tables()
                .unwrap()
                .map(|table| table.name().to_owned())
                .collect::<Vec<_>>(),
            ["geosite_other", "geosite_test", "metadata"]
        );
        assert!(read.list_multimap_tables().unwrap().next().is_none());
        {
            let meta = read.open_table(META).unwrap();
            assert_eq!(meta.get("schema").unwrap().unwrap().value(), "3");
            assert!(meta.get("legacy-only").unwrap().is_none());
            let digest: String = Sha256::digest(yaml.as_bytes())
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            assert_eq!(meta.get("sha256").unwrap().unwrap().value(), digest);
        }
        drop(read);
        drop(db);
        assert!(
            RedbDatabase::open(&cfg)
                .unwrap()
                .find_domain("test@telemetry", "tree.example")
                .unwrap()
        );
    }
}

#[test]
fn geosite_rebuild_reclaims_space_from_legacy_tables() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path(), RouterDBKind::Geosite);
    legacy_geosite(&cfg, "2");
    {
        let db = Database::open(cfg.path()).unwrap();
        let write = db.begin_write().unwrap();
        {
            let mut table = write.open_table(RULES).unwrap();
            let padding = "x".repeat(1024);
            for index in 0..2048 {
                table
                    .insert(format!("{padding}{index:04}").as_str(), ())
                    .unwrap();
            }
        }
        write.commit().unwrap();
    }
    let old_size = cfg.path().metadata().unwrap().len();
    let source = dir.path().join("source.yml");
    std::fs::write(&source, YAML).unwrap();
    let db = RedbDatabase::import(&cfg, &source).unwrap();
    let new_size = cfg.path().metadata().unwrap().len();
    assert!(
        new_size < old_size / 4,
        "rebuilding must reclaim legacy pages: {old_size} -> {new_size} bytes"
    );
    assert!(db.find_domain("test@ads", "tree.example").unwrap());
    assert!(!db.find_domain("test", "old.example").unwrap());
    drop(db);
    assert!(
        RedbDatabase::open(&cfg)
            .unwrap()
            .find_domain("test", "exact.example")
            .unwrap()
    );
}

#[test]
fn failed_geosite_upgrade_preserves_old_tables_and_is_retried_on_restart() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path(), RouterDBKind::Geosite);
    legacy_geosite(&cfg, "2");
    let source = dir.path().join("bad.yml");
    std::fs::write(&source, "lists: [{name: test, rules: ['regexp:(']}]").unwrap();
    for _ in 0..2 {
        let mut inbounds = HashMap::new();
        let manager = Databases::build(std::slice::from_ref(&cfg), &mut inbounds).unwrap();
        assert!(inbounds.contains_key("db"));
        let lua = mlua::Lua::new();
        manager.install(&lua).unwrap();
        assert!(
            lua.load("find_domain('db', 'test', 'old.example')")
                .eval::<bool>()
                .unwrap_err()
                .to_string()
                .contains("schema upgrade download pending")
        );
        assert!(RedbDatabase::import(&cfg, &source).is_err());
        let db = Database::open(cfg.path()).unwrap();
        let read = db.begin_read().unwrap();
        assert_eq!(
            read.open_table(META)
                .unwrap()
                .get("schema")
                .unwrap()
                .unwrap()
                .value(),
            "2"
        );
        assert!(
            read.open_table(RULES)
                .unwrap()
                .get("old")
                .unwrap()
                .is_some()
        );
        assert_eq!(read.list_multimap_tables().unwrap().count(), 1);
        let old = read
            .open_table(TableDefinition::<(u8, &[u8]), u8>::new("geosite_test"))
            .unwrap();
        assert_eq!(
            old.get((1, b"old.example".as_slice()))
                .unwrap()
                .unwrap()
                .value(),
            3
        );
    }
}

#[test]
fn geosite_schema_three_requires_byte_tuple_keys_and_u32_values() {
    for wrong_type in ["missing", "keys", "values"] {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path(), RouterDBKind::Geosite);
        let db = Database::create(cfg.path()).unwrap();
        let write = db.begin_write().unwrap();
        {
            let mut meta = write.open_table(META).unwrap();
            for (key, value) in [
                ("schema", "3"),
                ("type", "geosite"),
                ("url", cfg.url()),
                ("sha256", "test"),
            ] {
                meta.insert(key, value).unwrap();
            }
            if wrong_type == "keys" {
                write
                    .open_table(TableDefinition::<&str, ()>::new("geosite_test"))
                    .unwrap();
            } else if wrong_type == "values" {
                write
                    .open_table(TableDefinition::<(u8, &[u8]), u8>::new("geosite_test"))
                    .unwrap();
            } else {
                write.open_table(RULES).unwrap();
            }
        }
        write.commit().unwrap();
        drop(db);
        assert!(RedbDatabase::open(&cfg).is_err());
    }
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
        (RouterDBKind::Geosite, "4", "3"),
        (RouterDBKind::Geosite, "unknown", "3"),
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
                    list_include: Vec::new(),
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
    for version in [None, Some("1"), Some("2")] {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path(), RouterDBKind::Geosite);
        // Use HTTP locally while still asserting that no direct connection is made:
        // the test serves all responses over the emitted proxy sessions.
        let mut cfg = cfg;
        let RouterDatabaseCfg::Geosite(source) = &mut cfg else {
            unreachable!()
        };
        source.url = "http://download.test/start".into();
        if let Some(version) = version {
            legacy_geosite(&cfg, version);
        }
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
            assert!(matches!(inbound.accept().await, Err(SError::InboundClosed)));
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
        assert!(matches!(inbound.accept().await, Err(SError::InboundClosed)));
    })
    .await
    .unwrap();
    assert!(
        databases.slots["db"]
            .value
            .read()
            .unwrap()
            .as_ref()
            .err()
            .unwrap()
            .contains("404")
    );
    assert!(!cfg.path().exists());
    inbound.shutdown().await.unwrap();
}

#[test]
fn import_hashes_sources_larger_than_the_read_buffer() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path(), RouterDBKind::Geosite);
    let source = dir.path().join("source.yml");
    let content = format!("{}\n{YAML}", "#".repeat(9000));
    std::fs::write(&source, content).unwrap();
    let db = RedbDatabase::import(&cfg, &source).unwrap();
    let read = db.db.begin_read().unwrap();
    let meta = read.open_table(META).unwrap();
    assert_eq!(
        meta.get("sha256").unwrap().unwrap().value(),
        "bc781155d1a56d96fdcf0adfdb7a81ad2960820aff2730a9dc7e57aa32e0467c"
    );
}

#[test]
fn geosite_list_selection_filters_tables_and_preserves_attributes() {
    for selected in [vec![], vec!["TEST", "test"], vec!["other", "TEST"]] {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = config(dir.path(), RouterDBKind::Geosite);
        let RouterDatabaseCfg::Geosite(site) = &mut cfg else {
            unreachable!()
        };
        site.list_include = selected.iter().map(|name| (*name).into()).collect();
        let source = dir.path().join("source.yml");
        std::fs::write(&source, YAML).unwrap();
        let db = RedbDatabase::import(&cfg, &source).unwrap();
        assert!(db.find_domain("test", "exact.example").unwrap());
        assert!(db.find_domain("test@ads", "tree.example").unwrap());
        let includes_other = selected.is_empty() || selected.contains(&"other");
        assert_eq!(
            db.find_domain("other", "other.example").unwrap(),
            includes_other
        );
        let read = db.db.begin_read().unwrap();
        let tables: Vec<_> = read
            .list_tables()
            .unwrap()
            .map(|table| table.name().to_owned())
            .collect();
        assert_eq!(
            tables.iter().any(|name| name == "geosite_other"),
            includes_other
        );
        drop(read);
        drop(db);
        let RouterDatabaseCfg::Geosite(site) = &mut cfg else {
            unreachable!()
        };
        site.list_include = selected
            .iter()
            .rev()
            .map(|name| name.to_ascii_lowercase())
            .collect();
        site.list_include.dedup();
        let db = RedbDatabase::open(&cfg).unwrap();
        assert!(db.find_domain("test@ads", "tree.example").unwrap());
        drop(db);
        let RouterDatabaseCfg::Geosite(site) = &mut cfg else {
            unreachable!()
        };
        site.list_include = vec!["other".into()];
        let error = RedbDatabase::open(&cfg).err().unwrap().to_string();
        assert!(error.contains("incompatible list selection"));
    }
}

#[test]
fn geosite_list_selection_rejects_unknown_names_without_publishing() {
    for selected in [
        vec!["missing"],
        vec!["test", "missing"],
        vec!["test@ads"],
        vec![""],
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = config(dir.path(), RouterDBKind::Geosite);
        let RouterDatabaseCfg::Geosite(site) = &mut cfg else {
            unreachable!()
        };
        site.list_include = selected.iter().map(|name| (*name).into()).collect();
        let source = dir.path().join("source.yml");
        std::fs::write(&source, YAML).unwrap();
        assert!(RedbDatabase::import(&cfg, &source).is_err());
        assert!(!cfg.path().exists());
    }
}

#[test]
fn geosite_legacy_unfiltered_metadata_remains_compatible() {
    let dir = tempfile::tempdir().unwrap();
    let (cfg, db) = geosite(dir.path());
    let write = db.db.begin_write().unwrap();
    write.open_table(META).unwrap().remove("list").unwrap();
    write.commit().unwrap();
    drop(db);
    let db = RedbDatabase::open(&cfg).unwrap();
    assert!(db.find_domain("test", "exact.example").unwrap());
    assert!(db.find_domain("other", "other.example").unwrap());
}
