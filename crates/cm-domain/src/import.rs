//! CSLB licence import.
//!
//! Reads an operator-supplied file from the official CSLB Public Data Portal.
//! Nothing here fetches from the network: the portal serves its downloads
//! through an ASP.NET postback rather than a stable URL, so automating it would
//! be a brittle dependency load-bearing on a cron nobody watches. The operator
//! downloads; this reads what they downloaded, and records exactly which bytes
//! it read.
//!
//! Three properties matter and are tested:
//!
//! * **Idempotent.** The same file twice changes nothing observable.
//! * **Raw-preserving.** Every changed row appends its source verbatim, so a
//!   mapping mistake is repairable without re-downloading.
//! * **Non-destructive.** Claimant-written fields are never overwritten.

use chrono::NaiveDate;
use cm_core::AppError;
use cm_db::repo::contractors::{self, SourceFacts};
use cm_db::repo::licenses::{
    self, LicenseRecord, LicenseStatus, RunCounts, RunStatus, Source, UpsertOutcome,
};
use cm_db::repo::{geocode, reference};
use cm_db::PgPool;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

/// Rows per transaction. Small enough that a failure mid-file leaves a truthful
/// partial run rather than an eight-minute transaction holding a connection.
pub const DEFAULT_BATCH: usize = 500;

#[derive(Debug, Clone)]
pub struct ImportOptions {
    pub source: Source,
    pub file_path: PathBuf,
    /// Restricts the import to one county, matched case-insensitively.
    pub county: Option<String>,
    pub snapshot_date: Option<NaiveDate>,
    pub batch_size: usize,
    /// Parse and count, write nothing.
    pub dry_run: bool,
}

/// The fields the importer needs, and the header names it will accept for each.
///
/// Tolerant on purpose. The exact column titles of the CSLB master file have
/// not been verified against a real download in this environment, so the
/// importer accepts the plausible spellings and — crucially — **fails loudly
/// listing the headers it actually saw** when a required field is missing,
/// rather than importing a file it has misunderstood.
const FIELD_ALIASES: &[(&str, &[&str])] = &[
    (
        "license_no",
        &["licenseno", "licensenumber", "license", "licnum", "lic"],
    ),
    (
        "business_name",
        &[
            "businessname",
            "name",
            "bizname",
            "dbaname",
            "doingbusinessas",
        ],
    ),
    ("status", &["primarystatus", "licensestatus", "status"]),
    ("business_type", &["businesstype", "entitytype", "biztype"]),
    ("issue_date", &["issuedate", "originalissuedate", "issuedt"]),
    (
        "expiration_date",
        &["expirationdate", "expdate", "expirationdt"],
    ),
    (
        "classifications",
        &[
            // The real Master List (verified 2026-08-24) names this column
            // "Classifications(s)" — CSLB's own typo. normalise_header strips
            // the parentheses, leaving a doubled s. Without this alias the
            // column is silently dropped: it is not in REQUIRED, so the import
            // succeeds and every contractor lands with no trades at all.
            "classificationss",
            "classifications",
            "classification",
            "class",
            "classificationcodes",
            "classcodes",
        ],
    ),
    (
        "address_line1",
        &[
            "mailingaddress",
            "address",
            "addressline1",
            "streetaddress",
            "businessaddress",
            "address1",
        ],
    ),
    ("city", &["city", "mailingcity", "businesscity"]),
    ("state", &["state", "mailingstate", "businessstate"]),
    (
        "postal_code",
        &["zipcode", "zip", "postalcode", "mailingzip", "businesszip"],
    ),
    ("county", &["county", "mailingcounty", "businesscounty"]),
    (
        "phone",
        &["businessphone", "phone", "phonenumber", "telephone"],
    ),
    (
        "bond_amount",
        &["bondamount", "cbamount", "contractorbondamount"],
    ),
    (
        "workers_comp",
        &[
            "workerscompcoveragetype",
            "wccoveragetype",
            "workerscompensation",
            "workerscomp",
        ],
    ),
];

/// Fields without which the file is not a licence register.
const REQUIRED: &[&str] = &["license_no", "business_name", "status"];

/// Normalise a header for matching: case, spaces and punctuation all vary
/// between CSLB's own exports.
fn normalise_header(header: &str) -> String {
    header
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Normalise a CSLB classification code for matching against the trade table.
///
/// CSLB is not consistent with itself. The Master List downloaded 2026-08-24
/// carries `C10`, `C36` and `C33` unhyphenated on most rows, and `C-7`, `C-8`,
/// `C-6` hyphenated on others — in the same column of the same file. Comparing
/// the raw strings matched only `B`, which left 6,942 electricians, 4,823
/// plumbers, 2,989 painters, 1,698 landscapers and 1,226 roofers with no trade
/// at all, and therefore invisible to every trade filter in the directory.
fn normalise_classification(code: &str) -> String {
    code.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

#[derive(Debug, Clone)]
struct ColumnMap {
    columns: HashMap<&'static str, usize>,
    headers: Vec<String>,
}

impl ColumnMap {
    fn build(headers: &csv::StringRecord) -> Result<Self, AppError> {
        let normalised: Vec<String> = headers.iter().map(normalise_header).collect();
        let mut columns = HashMap::new();

        for (field, aliases) in FIELD_ALIASES {
            if let Some(index) = normalised
                .iter()
                .position(|header| aliases.contains(&header.as_str()))
            {
                columns.insert(*field, index);
            }
        }

        let missing: Vec<&str> = REQUIRED
            .iter()
            .copied()
            .filter(|field| !columns.contains_key(field))
            .collect();

        if !missing.is_empty() {
            return Err(AppError::invalid(format!(
                "the file is missing required column(s) {missing:?}. Columns found: {:?}. \
                 If CSLB has renamed a column, add the new name to FIELD_ALIASES.",
                headers.iter().collect::<Vec<_>>()
            )));
        }

        Ok(Self {
            columns,
            headers: headers.iter().map(str::to_owned).collect(),
        })
    }

    fn get<'a>(&self, row: &'a csv::StringRecord, field: &str) -> Option<&'a str> {
        self.columns
            .get(field)
            .and_then(|index| row.get(*index))
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }
}

/// Map CSLB's status vocabulary onto ours. The original string is stored
/// alongside, so an unrecognised value is visible rather than lost.
fn map_status(raw: &str) -> LicenseStatus {
    let upper = raw.to_ascii_uppercase();

    // Order matters, and getting it wrong is not cosmetic: "INACTIVE" contains
    // "ACTIVE", so testing for active first maps every inactive licence to
    // active — and the verified badge is computed from exactly this value. The
    // negative and specific cases are therefore tested before the positive one.
    if upper.contains("INACTIVE") {
        LicenseStatus::Inactive
    } else if upper.contains("SUSP") {
        // "SUSP", not "SUSPEND". CSLB abbreviates: the real Master List
        // (verified 2026-08-24) carries "CONTR BOND SUSP", "WORK COMP SUSP",
        // "LIAB INS SUSP" and twenty more like them, and not one contains
        // "SUSPEND". Even "SOS SUSPENSION" does not — it reads SUSPEN-S-ION.
        // Matching the long form sent 13,512 suspended licences to Unknown.
        LicenseStatus::Suspended
    } else if upper.contains("EXPIR") {
        LicenseStatus::Expired
    } else if upper.contains("CLEAR") || upper.contains("ACTIVE") {
        LicenseStatus::Active
    } else {
        LicenseStatus::Unknown
    }
}

/// CSLB exports have used more than one date format over time.
fn parse_date(value: &str) -> Option<NaiveDate> {
    for format in ["%Y-%m-%d", "%m/%d/%Y", "%d-%b-%Y", "%Y%m%d", "%m/%d/%y"] {
        if let Ok(date) = NaiveDate::parse_from_str(value, format) {
            return Some(date);
        }
    }
    None
}

fn parse_money_cents(value: &str) -> Option<i64> {
    let cleaned: String = value
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    if cleaned.is_empty() {
        return None;
    }
    cleaned
        .parse::<f64>()
        .ok()
        .filter(|amount| amount.is_finite() && *amount >= 0.0)
        .map(|amount| (amount * 100.0).round() as i64)
}

/// Split a classification cell. CSLB has used spaces, commas and slashes.
fn parse_classifications(value: &str) -> Vec<String> {
    value
        .split([',', ';', '/', ' ', '|'])
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(|part| part.to_ascii_uppercase())
        .collect()
}

fn normalise_postal_code(value: &str) -> Option<String> {
    let digits: String = value.chars().filter(char::is_ascii_digit).take(5).collect();
    (digits.len() == 5).then_some(digits)
}

/// The digest that decides whether a row has changed.
fn content_hash(record: &LicenseRecord) -> Vec<u8> {
    let mut hasher = Sha256::new();
    for field in [
        record.business_name.as_str(),
        record.business_type.as_deref().unwrap_or(""),
        record.status.as_str(),
        record.status_raw.as_str(),
        &record.issue_date.map(|d| d.to_string()).unwrap_or_default(),
        &record
            .expiration_date
            .map(|d| d.to_string())
            .unwrap_or_default(),
        &record.classifications.join(","),
        &record
            .bond_amount_cents
            .map(|c| c.to_string())
            .unwrap_or_default(),
        record.workers_comp_status.as_deref().unwrap_or(""),
        record.address_line1.as_deref().unwrap_or(""),
        record.city.as_deref().unwrap_or(""),
        record.state.as_deref().unwrap_or(""),
        record.postal_code.as_deref().unwrap_or(""),
        record.county.as_deref().unwrap_or(""),
        record.phone.as_deref().unwrap_or(""),
    ] {
        hasher.update(field.as_bytes());
        // A separator, so ("ab","c") and ("a","bc") cannot collide.
        hasher.update([0u8]);
    }
    hasher.finalize().to_vec()
}

/// The digest of the address a geocode job would resolve.
pub fn address_hash(address: &str) -> Vec<u8> {
    Sha256::digest(address.trim().to_lowercase().as_bytes()).to_vec()
}

/// SHA-256 of the file, read in chunks so a large file is never held in memory.
pub fn file_digest(path: &Path) -> Result<Vec<u8>, AppError> {
    let file = std::fs::File::open(path)
        .map_err(|e| AppError::invalid(format!("cannot read {}: {e}", path.display())))?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];

    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|e| AppError::invalid(format!("cannot read {}: {e}", path.display())))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    Ok(hasher.finalize().to_vec())
}

fn build_record(columns: &ColumnMap, row: &csv::StringRecord) -> Result<LicenseRecord, String> {
    let license_no = columns
        .get(row, "license_no")
        .ok_or("no licence number")?
        .to_owned();
    let business_name = columns
        .get(row, "business_name")
        .ok_or("no business name")?
        .to_owned();
    let status_raw = columns.get(row, "status").ok_or("no status")?.to_owned();

    // The source row verbatim, so a mapping mistake is repairable later.
    let raw = serde_json::Value::Object(
        columns
            .headers
            .iter()
            .enumerate()
            .map(|(index, header)| {
                (
                    header.clone(),
                    serde_json::Value::String(row.get(index).unwrap_or("").to_owned()),
                )
            })
            .collect(),
    );

    let mut record = LicenseRecord {
        status: map_status(&status_raw),
        license_no,
        business_name,
        business_type: columns.get(row, "business_type").map(str::to_owned),
        status_raw,
        issue_date: columns.get(row, "issue_date").and_then(parse_date),
        expiration_date: columns.get(row, "expiration_date").and_then(parse_date),
        classifications: columns
            .get(row, "classifications")
            .map(parse_classifications)
            .unwrap_or_default(),
        bond_amount_cents: columns.get(row, "bond_amount").and_then(parse_money_cents),
        workers_comp_status: columns.get(row, "workers_comp").map(str::to_owned),
        address_line1: columns.get(row, "address_line1").map(str::to_owned),
        city: columns.get(row, "city").map(str::to_owned),
        state: columns
            .get(row, "state")
            .map(|s| s.to_ascii_uppercase().chars().take(2).collect()),
        postal_code: columns
            .get(row, "postal_code")
            .and_then(normalise_postal_code),
        county: columns.get(row, "county").map(str::to_owned),
        phone: columns.get(row, "phone").map(str::to_owned),
        raw,
        content_hash: Vec::new(),
    };

    record.content_hash = content_hash(&record);
    Ok(record)
}

/// Run an import.
pub async fn run(pool: &PgPool, options: &ImportOptions) -> Result<RunCounts, AppError> {
    let digest = file_digest(&options.file_path)?;
    let file_name = options
        .file_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| options.file_path.display().to_string());

    let file = std::fs::File::open(&options.file_path).map_err(|e| {
        AppError::invalid(format!("cannot read {}: {e}", options.file_path.display()))
    })?;
    // Streamed, never slurped: the master file is hundreds of thousands of rows.
    let mut reader = csv::ReaderBuilder::new()
        .flexible(true)
        .from_reader(BufReader::new(file));

    let headers = reader
        .headers()
        .map_err(|e| AppError::invalid(format!("cannot read the header row: {e}")))?
        .clone();
    let columns = ColumnMap::build(&headers)?;

    if options.county.is_some() && !columns.columns.contains_key("county") {
        return Err(AppError::invalid(
            "a county filter was requested but the file has no county column; \
             re-run without --county to import every row",
        ));
    }

    // Every trade, not just the featured ones: a licence classified C-11 must
    // still import as an elevator contractor even though the directory does not
    // offer "Elevator" as a filter.
    let trades = {
        let mut conn = pool.acquire().await.map_err(AppError::internal)?;
        reference::all_trades_for_import(&mut conn).await?
    };
    let trade_by_classification: HashMap<String, uuid::Uuid> = trades
        .iter()
        .filter_map(|trade| {
            trade
                .cslb_classification
                .as_ref()
                .map(|c| (normalise_classification(c), trade.id))
        })
        .collect();

    let run_id = {
        let mut conn = pool.acquire().await.map_err(AppError::internal)?;
        licenses::begin_run(
            &mut conn,
            options.source,
            &file_name,
            &digest,
            options.snapshot_date,
        )
        .await?
    };

    let outcome = import_rows(
        pool,
        run_id,
        options,
        &columns,
        &mut reader,
        &trade_by_classification,
    )
    .await;

    let mut conn = pool.acquire().await.map_err(AppError::internal)?;
    match outcome {
        Ok(counts) => {
            let status = if options.dry_run {
                // A dry run must not occupy the "these bytes were imported"
                // slot, or the real import afterwards would be refused.
                RunStatus::Failed
            } else {
                RunStatus::Succeeded
            };
            let note = options.dry_run.then_some("dry run: nothing was written");
            licenses::finish_run(&mut conn, run_id, status, counts, note).await?;
            Ok(counts)
        }
        Err(error) => {
            licenses::finish_run(
                &mut conn,
                run_id,
                RunStatus::Failed,
                RunCounts::default(),
                Some(&error.to_string()),
            )
            .await?;
            Err(error)
        }
    }
}

async fn import_rows<R: std::io::Read>(
    pool: &PgPool,
    run_id: uuid::Uuid,
    options: &ImportOptions,
    columns: &ColumnMap,
    reader: &mut csv::Reader<R>,
    trade_by_classification: &HashMap<String, uuid::Uuid>,
) -> Result<RunCounts, AppError> {
    let county = options
        .county
        .as_ref()
        .map(|c| c.trim().to_ascii_uppercase());
    let mut counts = RunCounts::default();
    let mut unmapped: std::collections::BTreeMap<String, u32> = std::collections::BTreeMap::new();
    let mut batch: Vec<LicenseRecord> = Vec::with_capacity(options.batch_size);

    for row in reader.records() {
        let row = match row {
            Ok(row) => row,
            Err(error) => {
                // A malformed row is counted and skipped, never silently
                // dropped and never fatal to the whole file.
                tracing::warn!(%error, "rejecting a malformed row");
                counts.rejected += 1;
                continue;
            }
        };
        counts.read += 1;

        if let Some(county) = &county {
            let row_county = columns
                .get(&row, "county")
                .map(|c| c.trim().to_ascii_uppercase())
                .unwrap_or_default();
            if &row_county != county {
                counts.skipped += 1;
                continue;
            }
        }

        match build_record(columns, &row) {
            Ok(record) => batch.push(record),
            Err(reason) => {
                tracing::warn!(reason, "rejecting a row");
                counts.rejected += 1;
            }
        }

        if batch.len() >= options.batch_size {
            flush(
                pool,
                run_id,
                options,
                &mut batch,
                trade_by_classification,
                &mut counts,
                &mut unmapped,
            )
            .await?;
        }
    }

    if !batch.is_empty() {
        flush(
            pool,
            run_id,
            options,
            &mut batch,
            trade_by_classification,
            &mut counts,
            &mut unmapped,
        )
        .await?;
    }

    // Say what was dropped. A classification with no trade means those
    // contractors match no trade filter at all, and the only way that was ever
    // visible before was to go looking for it.
    if !unmapped.is_empty() {
        let total: u32 = unmapped.values().sum();
        let mut worst: Vec<(&String, &u32)> = unmapped.iter().collect();
        worst.sort_by(|a, b| b.1.cmp(a.1));
        let sample: Vec<String> = worst
            .iter()
            .take(10)
            .map(|(code, count)| format!("{code}={count}"))
            .collect();

        tracing::warn!(
            distinct = unmapped.len(),
            occurrences = total,
            top = %sample.join(" "),
            "classifications with no matching trade; those contractors match no trade filter. \
             Add them to CANONICAL_TRADES if they should be searchable."
        );
    }

    Ok(counts)
}

/// Write one batch in a single transaction, then clear it.
async fn flush(
    pool: &PgPool,
    run_id: uuid::Uuid,
    options: &ImportOptions,
    batch: &mut Vec<LicenseRecord>,
    trade_by_classification: &HashMap<String, uuid::Uuid>,
    counts: &mut RunCounts,
    unmapped: &mut std::collections::BTreeMap<String, u32>,
) -> Result<(), AppError> {
    if options.dry_run {
        counts.inserted += batch.len() as i32;
        batch.clear();
        return Ok(());
    }

    let mut tx = pool.begin().await.map_err(AppError::internal)?;

    for record in batch.iter() {
        let stored = licenses::upsert(&mut tx, run_id, record).await?;
        match stored.outcome {
            UpsertOutcome::Inserted => counts.inserted += 1,
            UpsertOutcome::Updated => counts.updated += 1,
            UpsertOutcome::Unchanged => counts.unchanged += 1,
        }

        // Unchanged rows need no projection work: the contractor already
        // reflects them, and redoing it would move `updated_at` for nothing.
        if stored.outcome == UpsertOutcome::Unchanged {
            continue;
        }

        let region_id = match &record.postal_code {
            Some(code) => reference::find_zcta(&mut tx, code).await?.map(|r| r.id),
            None => None,
        };

        let slug_base = crate::slugify(&record.business_name);
        let slug_suffix = crate::slugify(&record.license_no);
        let slug = match (slug_base.is_empty(), slug_suffix.is_empty()) {
            (true, true) => format!("contractor-{}", stored.id.simple()),
            (true, false) => format!("contractor-{slug_suffix}"),
            (false, true) => format!("{slug_base}-{}", stored.id.simple()),
            (false, false) => format!("{slug_base}-{slug_suffix}"),
        };

        let upserted = contractors::upsert_from_license(
            &mut tx,
            &SourceFacts {
                license_record_id: stored.id,
                display_name: record.business_name.clone(),
                slug,
                postal_code: record.postal_code.clone(),
                region_id,
            },
        )
        .await?;

        // A classification with no trade is counted, not discarded. The
        // silent version of this line is how six seeded trades came to cover
        // 61% of the register without anything ever saying so.
        let mut trade_ids: Vec<uuid::Uuid> = Vec::new();
        for classification in &record.classifications {
            let normalised = normalise_classification(classification);
            match trade_by_classification.get(&normalised) {
                Some(id) => trade_ids.push(*id),
                None => *unmapped.entry(normalised).or_insert(0) += 1,
            }
        }
        contractors::replace_cslb_trades(&mut tx, upserted.id, &trade_ids).await?;

        // Publish a point immediately, so a contractor is searchable before any
        // geocoder has run — at the exact address if a previous run already
        // resolved one, and at the ZIP centroid otherwise.
        //
        // `republish` reads the stored precise point rather than being handed
        // one, which is what makes re-importing the CSLB export safe. The
        // function this replaced passed `None` and wiped it.
        if upserted.created || upserted.location_changed {
            crate::location::republish(&mut tx, upserted.id).await?;

            if let Some(address) = contractors::geocodable_address(&mut tx, upserted.id).await? {
                geocode::enqueue(&mut tx, upserted.id, &address_hash(&address)).await?;
            }
        }

        crate::verification::recompute(&mut tx, upserted.id, Some(run_id)).await?;
    }

    tx.commit().await.map_err(AppError::internal)?;
    batch.clear();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_are_matched_regardless_of_spelling() {
        for (canonical, variants) in [
            ("licenseno", ["License Number", "LICENSE_NO", "license no."]),
            (
                "businessname",
                ["Business Name", "BUSINESS-NAME", "business name"],
            ),
        ] {
            for variant in variants {
                let normalised = normalise_header(variant);
                assert!(
                    FIELD_ALIASES
                        .iter()
                        .any(|(_, aliases)| aliases.contains(&normalised.as_str())),
                    "{variant} normalised to {normalised}, which matches no alias"
                );
                let _ = canonical;
            }
        }
    }

    #[test]
    fn a_file_without_the_required_columns_is_refused_with_what_it_saw() {
        let headers = csv::StringRecord::from(vec!["Something", "Else"]);
        let error = ColumnMap::build(&headers).expect_err("should refuse");

        let message = error.to_string();
        assert!(message.contains("license_no"), "{message}");
        assert!(
            message.contains("Something"),
            "the message must name what it saw: {message}"
        );
    }

    #[test]
    fn statuses_map_onto_the_documented_set() {
        assert_eq!(map_status("CLEAR"), LicenseStatus::Active);
        assert_eq!(map_status("Active"), LicenseStatus::Active);
        assert_eq!(map_status("EXPIRED"), LicenseStatus::Expired);
        assert_eq!(map_status("SUSPENDED"), LicenseStatus::Suspended);
        assert_eq!(map_status("INACTIVE"), LicenseStatus::Inactive);
        assert_eq!(map_status("something new"), LicenseStatus::Unknown);

        // The trap: these substrings overlap, and the wrong order silently
        // marks dead licences as live.
        assert_eq!(map_status("INACTIVE"), LicenseStatus::Inactive);
        assert_eq!(map_status("Inactive - suspended"), LicenseStatus::Inactive);
        assert_eq!(map_status("EXPIRED - INACTIVE"), LicenseStatus::Inactive);
        assert_eq!(map_status("SUSPENDED"), LicenseStatus::Suspended);
    }

    /// The real vocabulary, taken from the Master List downloaded 2026-08-24.
    ///
    /// These are the values that actually occur, and none of them spells
    /// "SUSPEND" in full. This test exists because the previous one asserted
    /// only on the long form and therefore passed while 13,512 real licences —
    /// 7,972 of them with a suspended contractor bond — were being mapped to
    /// Unknown.
    #[test]
    fn the_real_cslb_suspension_vocabulary_maps_to_suspended() {
        for raw in [
            "CONTR BOND SUSP",
            "WORK COMP SUSP",
            "LIAB INS SUSP",
            "SUSP - NO QUALIFIER",
            "FAMILY SUP SUSP",
            "SOS SUSPENSION",
            "JUDGEMENT SUSP",
            "OUT LIAB SUSP",
            "BOND PAY SUSP",
            "EMP/WK BND SUSP",
            "JDG ENTITY SUSP",
            "QUAL BOND SUSP",
            "BND PAY EN SUSP",
            "O/L ENTITY SUSP",
        ] {
            assert_eq!(
                map_status(raw),
                LicenseStatus::Suspended,
                "{raw} must not be Unknown: the badge is derived from this value"
            );
        }

        // The one status that means the licence is good.
        assert_eq!(map_status("CLEAR"), LicenseStatus::Active);
    }

    /// CSLB writes the same classification both ways in one file, so matching
    /// has to be insensitive to the hyphen. The seeded trades use "C-10"; the
    /// real Master List overwhelmingly says "C10".
    #[test]
    fn classification_codes_match_regardless_of_hyphenation() {
        // Every spelling of the same code collapses to one key.
        for spelling in ["C-10", "C10", "c 10", " c-10 "] {
            assert_eq!(
                normalise_classification(spelling),
                "C10",
                "{spelling} must match the seeded C-10 trade"
            );
        }

        // Distinct codes must NOT collide: C-1 and C1 are the same trade, but
        // C-10 and C1 are different ones, and stripping must not merge them.
        assert_ne!(
            normalise_classification("C-10"),
            normalise_classification("C-1")
        );
        assert_eq!(normalise_classification("B"), "B");
        assert_eq!(normalise_classification("A"), "A");
    }

    /// CSLB's own header carries a typo, and stripping punctuation doubles the
    /// s. Classifications is not a REQUIRED column, so getting this wrong does
    /// not fail the import — it just leaves every contractor with no trades.
    #[test]
    fn the_real_classifications_header_is_recognised() {
        let headers = csv::StringRecord::from(vec![
            "LicenseNo",
            "BusinessName",
            "PrimaryStatus",
            "Classifications(s)",
        ]);
        let map = ColumnMap::build(&headers).expect("required columns are present");
        assert!(
            map.columns.contains_key("classifications"),
            "Classifications(s) must map, or every contractor imports with no trades"
        );
    }

    #[test]
    fn dates_parse_in_every_format_cslb_has_used() {
        for value in ["2024-03-15", "03/15/2024", "15-Mar-2024", "20240315"] {
            assert_eq!(
                parse_date(value),
                Some(NaiveDate::from_ymd_opt(2024, 3, 15).expect("date")),
                "{value}"
            );
        }
        assert_eq!(parse_date("not a date"), None);
    }

    #[test]
    fn money_becomes_whole_cents() {
        assert_eq!(parse_money_cents("$15,000.00"), Some(1_500_000));
        assert_eq!(parse_money_cents("25000"), Some(2_500_000));
        assert_eq!(parse_money_cents(""), None);
        assert_eq!(parse_money_cents("n/a"), None);
    }

    #[test]
    fn classifications_split_on_every_separator_seen() {
        assert_eq!(parse_classifications("B C-10"), vec!["B", "C-10"]);
        assert_eq!(parse_classifications("b,c-36"), vec!["B", "C-36"]);
        assert_eq!(parse_classifications("B | C-27"), vec!["B", "C-27"]);
        assert!(parse_classifications("   ").is_empty());
    }

    #[test]
    fn postal_codes_are_reduced_to_five_digits_or_dropped() {
        assert_eq!(
            normalise_postal_code("90042-1234"),
            Some("90042".to_owned())
        );
        assert_eq!(normalise_postal_code(" 90042 "), Some("90042".to_owned()));
        assert_eq!(normalise_postal_code("9004"), None);
        assert_eq!(normalise_postal_code("not a zip"), None);
    }

    #[test]
    fn the_content_hash_separates_its_fields() {
        let base = LicenseRecord {
            license_no: "1".into(),
            business_name: "ab".into(),
            business_type: Some("c".into()),
            status: LicenseStatus::Active,
            status_raw: "CLEAR".into(),
            issue_date: None,
            expiration_date: None,
            classifications: vec![],
            bond_amount_cents: None,
            workers_comp_status: None,
            address_line1: None,
            city: None,
            state: None,
            postal_code: None,
            county: None,
            phone: None,
            raw: serde_json::json!({}),
            content_hash: vec![],
        };
        let mut shifted = base.clone();
        shifted.business_name = "a".into();
        shifted.business_type = Some("bc".into());

        assert_ne!(
            content_hash(&base),
            content_hash(&shifted),
            "field boundaries must be unambiguous"
        );
    }
}
