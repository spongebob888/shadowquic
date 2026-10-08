//! See document/ip-lookup-benchmark.md for methodology and usage.
use clap::{Parser, ValueEnum};
use maxminddb::{Reader, geoip2::Country};
use rand::{RngExt, SeedableRng, rngs::StdRng};
use shadowquic::{
    config::{CountryDbCfg, RouterDatabaseCfg},
    plugin::database::{RedbDatabase, Result, RouterDB},
};
use std::{
    fs,
    hint::black_box,
    io::{BufRead, BufReader, BufWriter, Write},
    net::IpAddr,
    path::{Path, PathBuf},
    process::Command,
    time::Instant,
};

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Family {
    V4,
    V6,
    Mixed,
}

#[derive(Parser, Debug)]
#[command(
    about = "Compare country membership lookup speed and process memory: MMDB vs ShadowQUIC redb"
)]
struct Args {
    /// Local country MMDB source; never downloaded or modified.
    #[arg(long)]
    mmdb: PathBuf,
    #[arg(long, default_value = "CN")]
    country: String,
    /// Generated query count (ignored with --ips).
    #[arg(long, default_value = "100000", value_parser = clap::value_parser!(u32).range(1..))]
    queries: u32,
    #[arg(long, default_value = "5", value_parser = clap::value_parser!(u32).range(1..))]
    rounds: u32,
    #[arg(long, default_value = "42")]
    seed: u64,
    #[arg(long, value_enum, default_value = "mixed")]
    family: Family,
    /// Workload file: one IP per line, allowing blank lines and # comments.
    /// Used verbatim; --family only controls generated workloads.
    #[arg(long)]
    ips: Option<PathBuf>,
    /// Directory for temporary redb and query files (defaults to OS temp directory).
    #[arg(long)]
    temp_dir: Option<PathBuf>,
    #[arg(long, hide = true)]
    worker: Option<String>,
    #[arg(long, hide = true)]
    work_dir: Option<PathBuf>,
}

fn config(dir: &Path) -> RouterDatabaseCfg {
    RouterDatabaseCfg::Country(CountryDbCfg {
        tag: "benchmark".into(),
        url: "https://benchmark.invalid/local.mmdb".into(),
        path: dir.join("country.redb"),
    })
}

fn mmdb_find(reader: &Reader<Vec<u8>>, country: &str, ip: IpAddr) -> Result<bool> {
    Ok(reader
        .lookup(ip)?
        .decode::<Country>()?
        .and_then(|record| record.country.iso_code)
        .is_some_and(|code| code.eq_ignore_ascii_case(country)))
}

fn prepare(args: &Args, dir: &Path) -> Result<()> {
    let reader = Reader::open_readfile(&args.mmdb)?;
    let ips: Vec<IpAddr> = if let Some(path) = &args.ips {
        BufReader::new(fs::File::open(path)?)
            .lines()
            .enumerate()
            .filter_map(|(i, line)| match line {
                Ok(line) => {
                    let line = line.split('#').next().unwrap().trim();
                    (!line.is_empty()).then(|| {
                        line.parse()
                            .map_err(|e| format!("IP line {}: {e}", i + 1).into())
                    })
                }
                Err(e) => Some(Err(e.into())),
            })
            .collect::<Result<_>>()?
    } else {
        let mut networks = [Vec::new(), Vec::new()];
        for entry in reader.networks(Default::default())? {
            let entry = entry?;
            if entry
                .decode::<Country>()?
                .and_then(|r| r.country.iso_code)
                .is_none()
            {
                continue;
            }
            let net = entry.network()?;
            networks[usize::from(net.ip().is_ipv6())].push((net.network(), net.broadcast()));
        }
        let mut rng = StdRng::seed_from_u64(args.seed);
        (0..args.queries)
            .map(|i| {
                let v6 = match args.family {
                    Family::V4 => false,
                    Family::V6 => true,
                    Family::Mixed => i % 2 == 1,
                };
                let nets = &networks[usize::from(v6)];
                // Half uniform addresses, half samples from country-bearing networks.
                // Use groups of four so both families get both distributions.
                if i % 4 < 2 && !nets.is_empty() {
                    let (start, end) = nets[rng.random_range(0..nets.len())];
                    match (start, end) {
                        (IpAddr::V4(start), IpAddr::V4(end)) => IpAddr::from(
                            rng.random_range(u32::from(start)..=u32::from(end))
                                .to_be_bytes(),
                        ),
                        (IpAddr::V6(start), IpAddr::V6(end)) => IpAddr::from(
                            rng.random_range(u128::from(start)..=u128::from(end))
                                .to_be_bytes(),
                        ),
                        _ => unreachable!(),
                    }
                } else if v6 {
                    IpAddr::from(rng.random::<u128>().to_be_bytes())
                } else {
                    IpAddr::from(rng.random::<u32>().to_be_bytes())
                }
            })
            .collect()
    };
    if ips.is_empty() {
        return Err("workload contains no IP addresses".into());
    }
    let mut out = BufWriter::new(fs::File::create(dir.join("queries"))?);
    let mut hits = 0;
    for &ip in &ips {
        let expected = mmdb_find(&reader, &args.country, ip)?;
        hits += usize::from(expected);
        writeln!(out, "{ip} {expected}")?;
    }
    out.flush()?;
    eprintln!(
        "workload: {} IPs ({} IPv4, {} IPv6), country={}, matches={}, seed={}",
        ips.len(),
        ips.iter().filter(|ip| ip.is_ipv4()).count(),
        ips.iter().filter(|ip| ip.is_ipv6()).count(),
        args.country,
        hits,
        args.seed
    );
    if hits == 0 || hits == ips.len() {
        eprintln!(
            "note: workload covers only one membership outcome; use --ips or another --country for mixed hits/misses"
        );
    }
    Ok(())
}

// Linux reports KiB despite labeling these fields kB. Elsewhere report NA.
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

fn measure<F: Fn(IpAddr) -> Result<bool>>(
    args: &Args,
    queries: &[(IpAddr, bool)],
    find: F,
    baseline: &str,
    open_ms: f64,
    opened: &str,
    file_bytes: u64,
) -> Result<()> {
    // Exact per-query verification also warms both backends before timing.
    for &(ip, expected) in queries {
        if find(ip)? != expected {
            return Err(format!(
                "{} disagrees with MMDB for {ip}",
                args.worker.as_deref().unwrap()
            )
            .into());
        }
    }
    for round in 1..=args.rounds {
        let start = Instant::now();
        let mut hits = 0usize;
        for &(ip, _) in queries {
            hits += usize::from(black_box(find(black_box(ip))?));
        }
        let elapsed = start.elapsed().as_secs_f64();
        let (rss, peak) = memory();
        println!(
            "{},{round},{},{hits},{:.3},{:.1},{open_ms:.3},{file_bytes},{baseline},{opened},{rss},{peak}",
            args.worker.as_deref().unwrap(),
            queries.len(),
            elapsed * 1e9 / queries.len() as f64,
            queries.len() as f64 / elapsed,
        );
    }
    Ok(())
}

fn worker(args: &Args) -> Result<()> {
    let dir = args.work_dir.as_ref().ok_or("missing worker directory")?;
    let cfg = config(dir);
    if args.worker.as_deref() == Some("import") {
        let start = Instant::now();
        let db = RedbDatabase::import(&cfg, &args.mmdb)?;
        eprintln!(
            "redb conversion: {:.3}s (excluded from lookup memory/timing)",
            start.elapsed().as_secs_f64()
        );
        drop(db);
        return Ok(());
    }
    let queries = BufReader::new(fs::File::open(dir.join("queries"))?)
        .lines()
        .map(|line| {
            let line = line?;
            let (ip, expected) = line.split_once(' ').ok_or("invalid query")?;
            Ok((ip.parse()?, expected.parse()?))
        })
        .collect::<Result<Vec<(IpAddr, bool)>>>()?;
    let (baseline, _) = memory();
    let start = Instant::now();
    match args.worker.as_deref() {
        Some("mmdb") => {
            let reader = Reader::open_readfile(&args.mmdb)?;
            let open_ms = start.elapsed().as_secs_f64() * 1000.0;
            let (opened, _) = memory();
            measure(
                args,
                &queries,
                |ip| mmdb_find(&reader, &args.country, ip),
                &baseline,
                open_ms,
                &opened,
                fs::metadata(&args.mmdb)?.len(),
            )?;
        }
        Some("redb") => {
            let db = RedbDatabase::open(&cfg)?;
            let open_ms = start.elapsed().as_secs_f64() * 1000.0;
            let (opened, _) = memory();
            measure(
                args,
                &queries,
                |ip| db.find_ip(&args.country, ip),
                &baseline,
                open_ms,
                &opened,
                fs::metadata(cfg.path())?.len(),
            )?;
        }
        _ => return Err("unknown worker".into()),
    }
    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();
    if args.country.len() != 2 || !args.country.bytes().all(|b| b.is_ascii_alphabetic()) {
        return Err("--country must be a two-letter ISO country code".into());
    }
    if args.worker.is_some() {
        return worker(&args);
    }
    if cfg!(debug_assertions) {
        eprintln!("warning: debug build; use --release for meaningful measurements");
    }
    let dir = match &args.temp_dir {
        Some(path) => tempfile::tempdir_in(path)?,
        None => tempfile::tempdir()?,
    };
    prepare(&args, dir.path())?;
    println!(
        "backend,round,lookups,hits,ns_per_lookup,lookups_per_sec,open_ms,file_bytes,baseline_rss_kib,opened_rss_kib,rss_kib,peak_rss_kib"
    );
    std::io::stdout().flush()?;
    for backend in ["import", "mmdb", "redb"] {
        let status = Command::new(std::env::current_exe()?)
            .args(std::env::args_os().skip(1))
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
