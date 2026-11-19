//! Inspect MMDB metadata, raw records, and compatibility with country lookups.
//! See document/mmdb-formats.md for usage and record formats.
use clap::Parser;
use maxminddb::{LookupResult, Reader};
use serde_json::{Value, json};
use shadowquic::plugin::database::MmdbCountryRecord;
use std::{error::Error, net::IpAddr, path::PathBuf, process::ExitCode};

#[derive(Debug, Parser)]
#[command(about = "Inspect country, GeoLite2, and Mihomo MMDB record formats")]
struct Args {
    /// Local MMDB files; filenames do not determine their record format.
    #[arg(required = true)]
    files: Vec<PathBuf>,
    /// IPs to look up in every file (repeat this option or separate with commas).
    #[arg(
        long,
        value_delimiter = ',',
        default_value = "1.1.1.1,8.8.8.8,223.5.5.5,192.168.1.1,2001:4860:4860::8888"
    )]
    ip: Vec<IpAddr>,
    /// Also decode this many networks, useful when the requested IPs miss.
    #[arg(long, default_value_t = 3)]
    samples: usize,
}

fn record(result: LookupResult<'_, Vec<u8>>) -> Result<Value, Box<dyn Error>> {
    // A dynamic value preserves maps, strings, and arrays instead of assuming
    // that every MMDB stores MaxMind's nested country object.
    let raw = result.decode::<Value>()?;
    // Exercise the same decoder used by Shadowquic's country router.
    let country = match result.decode::<MmdbCountryRecord>() {
        Ok(value) => json!({
            "codes": value.map(|record| record.codes().map(str::to_owned).collect::<Vec<_>>()).unwrap_or_default(),
        }),
        Err(error) => json!({ "error": error.to_string() }),
    };
    Ok(json!({
        "network": result.network()?.to_string(),
        "found": result.has_data(),
        "raw": raw,
        "shadowquic_country_decode": country,
    }))
}

fn inspect(path: &PathBuf, args: &Args) -> Result<Value, Box<dyn Error>> {
    let reader = Reader::open_readfile(path)?;
    let mut lookups = Vec::new();
    for &ip in &args.ip {
        if ip.is_ipv6() && reader.metadata().ip_version == 4 {
            lookups.push(json!({ "ip": ip, "status": "IPv6 unsupported by IPv4 database" }));
            continue;
        }
        let mut lookup = record(reader.lookup(ip)?)?;
        lookup["ip"] = json!(ip);
        lookups.push(lookup);
    }
    let mut samples = Vec::new();
    if args.samples > 0 {
        for entry in reader.networks(Default::default())?.take(args.samples) {
            samples.push(record(entry?)?);
        }
    }
    Ok(json!({
        "file": path,
        "metadata": reader.metadata(),
        "lookups": lookups,
        "samples": samples,
    }))
}

fn main() -> ExitCode {
    let args = Args::parse();
    let mut failed = false;
    let results: Vec<_> = args
        .files
        .iter()
        .map(|path| match inspect(path, &args) {
            Ok(value) => value,
            Err(error) => {
                failed = true;
                json!({ "file": path, "error": error.to_string() })
            }
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&results).unwrap());
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
