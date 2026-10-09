//! Compare ShadowQUIC's redb geosite search with a precompiled in-memory scan.
use clap::Parser;
use rand::{RngExt, SeedableRng, rngs::StdRng};
use serde::{Deserialize, Serialize};
use shadowquic::{
    config::{GeositeDbCfg, RouterDatabaseCfg},
    plugin::database::{RedbDatabase, Result, RouterDB},
};
use std::{
    fs,
    hint::black_box,
    io::{BufRead, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
    process::Command,
    time::Instant,
};

#[derive(Parser, Debug)]
#[command(about = "Compare geosite domain search: redb index vs precompiled in-memory rule scan")]
struct Args {
    /// Local v2fly geosite YAML; left unchanged.
    #[arg(long)]
    source: PathBuf,
    #[arg(long, default_value = "google")]
    list: String,
    /// Generated query count (ignored with --domains).
    #[arg(long, default_value = "100000", value_parser = clap::value_parser!(u32).range(1..))]
    queries: u32,
    #[arg(long, default_value = "5", value_parser = clap::value_parser!(u32).range(1..))]
    rounds: u32,
    #[arg(long, default_value = "42")]
    seed: u64,
    /// One domain per line, with optional # comments; preserves order and duplicates.
    #[arg(long)]
    domains: Option<PathBuf>,
    /// Choose temporary redb placement (defaults to OS temp directory).
    #[arg(long)]
    temp_dir: Option<PathBuf>,
    #[arg(long, hide = true)]
    worker: Option<String>,
    #[arg(long, hide = true)]
    work_dir: Option<PathBuf>,
}

#[derive(Debug, Deserialize, Serialize)]
struct Geosite {
    lists: Vec<SiteList>,
}

#[derive(Debug, Deserialize, Serialize)]
struct SiteList {
    name: String,
    #[serde(default)]
    rules: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Full,
    Domain,
    Keyword,
    Regex,
}

struct Rule {
    kind: Kind,
    value: String,
    regex: Option<regex::Regex>,
    attribute: String,
}

fn parse_rules(list: &SiteList) -> Result<Vec<Rule>> {
    list.rules
        .iter()
        .map(|rule| {
            let (kind, value) = rule.split_once(':').ok_or("geosite rule has no type")?;
            let (value, attribute) = value.split_once(":@").unwrap_or((value, ""));
            let kind = match kind {
                "full" => Kind::Full,
                "domain" => Kind::Domain,
                "keyword" => Kind::Keyword,
                "regexp" => Kind::Regex,
                _ => return Err(format!("unsupported geosite rule type: {kind}").into()),
            };
            let value = if matches!(kind, Kind::Regex) {
                value.to_owned()
            } else {
                value.to_ascii_lowercase()
            };
            let regex = matches!(kind, Kind::Regex)
                .then(|| regex::Regex::new(&value))
                .transpose()?;
            Ok(Rule {
                kind,
                value,
                regex,
                attribute: attribute.to_ascii_lowercase(),
            })
        })
        .collect()
}

fn memory_find(rules: &[Rule], list: &str, domain: &str) -> bool {
    let requested_attribute = list
        .split_once('@')
        .map(|(_, attr)| attr.to_ascii_lowercase());
    if requested_attribute
        .as_deref()
        .is_some_and(|attribute| !["ads", "!cn", "cn"].contains(&attribute))
    {
        return false;
    }
    let domain = domain.trim_end_matches('.').to_ascii_lowercase();
    for kind in [Kind::Full, Kind::Domain, Kind::Keyword, Kind::Regex] {
        for rule in rules.iter().filter(|rule| {
            rule.kind == kind
                && requested_attribute
                    .as_ref()
                    .is_none_or(|attribute| attribute == &rule.attribute)
        }) {
            let found = match kind {
                Kind::Full => domain == rule.value,
                Kind::Domain => {
                    domain == rule.value
                        || domain
                            .strip_suffix(&rule.value)
                            .is_some_and(|prefix| prefix.ends_with('.'))
                }
                Kind::Keyword => domain.contains(&rule.value),
                Kind::Regex => rule.regex.as_ref().unwrap().is_match(&domain),
            };
            if found {
                return true;
            }
        }
    }
    false
}

fn candidate(rule: &str) -> Option<String> {
    let (kind, body) = rule.split_once(':')?;
    let (value, _) = body.split_once(":@").unwrap_or((body, ""));
    match kind {
        "full" => Some(value.to_ascii_lowercase()),
        "domain" => Some(format!("www.{}", value.to_ascii_lowercase())),
        "keyword" => Some(format!("www.{}.example", value.to_ascii_lowercase())),
        "regexp" => [
            "www.google.com",
            "mail.google.com",
            "example.com",
            "www.example.com",
            "www.youtube.com",
            "www.facebook.com",
            "www.github.com",
        ]
        .into_iter()
        .find(|domain| regex::Regex::new(value).is_ok_and(|regex| regex.is_match(domain)))
        .map(str::to_owned),
        _ => None,
    }
}

fn prepare(args: &Args, dir: &Path, list: &SiteList, rules: &[Rule]) -> Result<()> {
    let domains = if let Some(path) = &args.domains {
        BufReader::new(fs::File::open(path)?)
            .lines()
            .enumerate()
            .filter_map(|(index, line)| match line {
                Ok(line) => {
                    let domain = line.split('#').next().unwrap().trim();
                    (!domain.is_empty()).then(|| {
                        if domain.contains('\0') {
                            Err(format!("NUL in domain line {}", index + 1).into())
                        } else {
                            Ok(domain.to_owned())
                        }
                    })
                }
                Err(error) => Some(Err(error.into())),
            })
            .collect::<Result<Vec<_>>>()?
    } else {
        let candidates: Vec<_> = list
            .rules
            .iter()
            .filter_map(|rule| candidate(rule))
            .collect();
        let hits: Vec<_> = candidates
            .iter()
            .filter(|domain| memory_find(rules, &args.list, domain))
            .cloned()
            .collect();
        if hits.is_empty() {
            return Err("could not generate a matching domain for this list; use --domains".into());
        }
        let mut rng = StdRng::seed_from_u64(args.seed);
        (0..args.queries)
            .map(|i| {
                if i % 2 == 0 {
                    hits[rng.random_range(0..hits.len())].clone()
                } else {
                    format!("miss-{i}-{}.invalid", rng.random::<u32>())
                }
            })
            .collect()
    };
    if domains.is_empty() {
        return Err("workload contains no domains".into());
    }
    let hits = domains
        .iter()
        .filter(|domain| memory_find(rules, &args.list, domain))
        .count();
    let mut output = BufWriter::new(fs::File::create(dir.join("queries"))?);
    for domain in &domains {
        writeln!(
            output,
            "{domain}\t{}",
            memory_find(rules, &args.list, domain)
        )?;
    }
    output.flush()?;
    let yaml = serde_saphyr::to_string(&Geosite {
        lists: vec![SiteList {
            name: list.name.clone(),
            rules: list.rules.clone(),
        }],
    })?;
    fs::write(dir.join("selected-list.yml"), yaml)?;
    eprintln!(
        "workload: {} domains, {} matches, list={}, rules={}, seed={}",
        domains.len(),
        hits,
        args.list,
        rules.len(),
        args.seed
    );
    Ok(())
}

fn memory() -> (String, String) {
    let status = fs::read_to_string("/proc/self/status").unwrap_or_default();
    let field = |name: &str| {
        status
            .lines()
            .find_map(|line| {
                line.strip_prefix(name)?
                    .split_whitespace()
                    .next()
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| "NA".into())
    };
    (field("VmRSS:"), field("VmHWM:"))
}

fn run_worker(args: &Args) -> Result<()> {
    let dir = args.work_dir.as_ref().ok_or("missing worker directory")?;
    let queries: Vec<(String, bool)> = BufReader::new(fs::File::open(dir.join("queries"))?)
        .lines()
        .map(|line| {
            let line = line?;
            let (domain, expected) = line.split_once('\t').ok_or("invalid query")?;
            Ok((domain.to_owned(), expected.parse()?))
        })
        .collect::<Result<_>>()?;
    let (baseline, _) = memory();
    let start = Instant::now();
    let rules = if args.worker.as_deref() == Some("memory") {
        let source = fs::read_to_string(dir.join("selected-list.yml"))?;
        let data: Geosite = serde_saphyr::from_str(&source)?;
        Some(parse_rules(
            data.lists.first().ok_or("empty selected list")?,
        )?)
    } else {
        None
    };
    let cfg = RouterDatabaseCfg::Geosite(GeositeDbCfg {
        tag: "benchmark".into(),
        url: "https://benchmark.invalid/geosite.yml".into(),
        path: dir.join("geosite.redb"),
    });
    let redb = if args.worker.as_deref() == Some("redb") {
        Some(RedbDatabase::open(&cfg)?)
    } else {
        None
    };
    let open_ms = start.elapsed().as_secs_f64() * 1000.0;
    let (opened, _) = memory();
    let find = |domain: &str| -> Result<bool> {
        if let Some(rules) = &rules {
            Ok(memory_find(rules, &args.list, domain))
        } else {
            redb.as_ref().unwrap().find_domain(&args.list, domain)
        }
    };
    for (domain, expected) in &queries {
        if find(domain)? != *expected {
            return Err(format!(
                "{} disagrees with in-memory reference for {domain}",
                args.worker.as_deref().unwrap()
            )
            .into());
        }
    }
    for round in 1..=args.rounds {
        let start = Instant::now();
        let mut hits = 0;
        for (domain, _) in &queries {
            hits += usize::from(black_box(find(black_box(domain))?));
        }
        let seconds = start.elapsed().as_secs_f64();
        let (rss, peak) = memory();
        println!(
            "{},{round},{},{hits},{:.3},{:.1},{open_ms:.3},{baseline},{opened},{rss},{peak}",
            args.worker.as_deref().unwrap(),
            queries.len(),
            seconds * 1e9 / queries.len() as f64,
            queries.len() as f64 / seconds
        );
    }
    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();
    if args.worker.is_some() {
        return run_worker(&args);
    }
    let source = fs::read_to_string(&args.source)?;
    let data: Geosite = serde_saphyr::from_str(&source)?;
    let mut lists = data
        .lists
        .into_iter()
        .filter(|list| list.name.eq_ignore_ascii_case(&args.list));
    let mut list = lists
        .next()
        .ok_or_else(|| format!("geosite list not found: {}", args.list))?;
    list.rules.extend(lists.flat_map(|list| list.rules));
    let rules = parse_rules(&list)?;
    let dir = match &args.temp_dir {
        Some(path) => tempfile::tempdir_in(path)?,
        None => tempfile::tempdir()?,
    };
    prepare(&args, dir.path(), &list, &rules)?;
    let cfg = RouterDatabaseCfg::Geosite(GeositeDbCfg {
        tag: "benchmark".into(),
        url: "https://benchmark.invalid/geosite.yml".into(),
        path: dir.path().join("geosite.redb"),
    });
    let start = Instant::now();
    drop(RedbDatabase::import(&cfg, &args.source)?);
    eprintln!(
        "redb import: {:.3}s (excluded from lookup timing)",
        start.elapsed().as_secs_f64()
    );
    println!(
        "backend,round,lookups,hits,ns_per_lookup,lookups_per_sec,open_ms,baseline_rss_kib,opened_rss_kib,rss_kib,peak_rss_kib"
    );
    std::io::stdout().flush()?;
    for backend in ["memory", "redb"] {
        let status = Command::new(std::env::current_exe()?)
            .arg("--source")
            .arg(&args.source)
            .arg("--list")
            .arg(&args.list)
            .arg("--rounds")
            .arg(args.rounds.to_string())
            .arg("--seed")
            .arg(args.seed.to_string())
            .arg("--worker")
            .arg(backend)
            .arg("--work-dir")
            .arg(dir.path())
            .status()?;
        if !status.success() {
            return Err(format!("{backend} worker failed: {status}").into());
        }
    }
    Ok(())
}
