//! Offline per-request API-equivalent token pricing. Rates never identify backend aliases.
use crate::model::UsageEvent;
use anyhow::{bail, Context, Result};
use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::BTreeSet, fs, path::Path};

const BUNDLE: &str = include_str!("../data/pricing.toml");

#[derive(Debug, Clone, Serialize)]
pub struct Estimate {
    pub cost: Option<Decimal>,
    pub status: String,
    pub assumptions: Vec<String>,
    pub reason: Option<String>,
    pub rate_id: Option<String>,
}
impl Estimate {
    fn unpriced(
        reason: impl Into<String>,
        assumptions: Vec<String>,
        rate_id: Option<String>,
    ) -> Self {
        Self {
            cost: None,
            status: "unpriced".into(),
            assumptions,
            reason: Some(reason.into()),
            rate_id,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Rate {
    #[serde(default)]
    id: String,
    model: String,
    #[serde(default = "openai")]
    provider: String,
    #[serde(default = "standard")]
    tier: String,
    #[serde(default)]
    from: Option<NaiveDate>,
    #[serde(default)]
    until: Option<NaiveDate>,
    #[serde(default)]
    min_input: Option<u64>,
    #[serde(default)]
    max_input: Option<u64>,
    input: Decimal,
    output: Decimal,
    #[serde(default)]
    cached: Option<Decimal>,
    #[serde(default)]
    cache_write: Option<Decimal>,
    #[serde(default)]
    cache_write_12h: Option<Decimal>,
    /// Explicitly verified provider/custom semantics. Absent means unverified.
    #[serde(default)]
    cache_write_mode: Option<String>,
    #[serde(default)]
    source: Option<String>,
}
fn openai() -> String {
    "openai".into()
}
fn standard() -> String {
    "standard".into()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Table {
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    verified: Option<NaiveDate>,
    #[serde(default)]
    source: Option<String>,
    rates: Vec<Rate>,
}

pub struct Pricer {
    bundled: Table,
    custom: Option<Table>,
    custom_path: Option<String>,
    custom_hash: Option<String>,
    repricing: Option<NaiveDate>,
    historical_only: bool,
    current_snapshot: bool,
}

fn tier(value: &str) -> &str {
    match value {
        "default" | "standard" => "standard",
        "priority" | "fast" => "fast",
        value => value,
    }
}
impl Rate {
    fn matches(&self, event: &UsageEvent, date: NaiveDate, tier_name: &str) -> bool {
        self.model == event.model
            && self.provider == event.provider
            && self.tier == tier_name
            && self.from.is_none_or(|bound| date >= bound)
            && self.until.is_none_or(|bound| date <= bound)
            && self
                .min_input
                .is_none_or(|bound| event.usage.input >= bound)
            && self
                .max_input
                .is_none_or(|bound| event.usage.input <= bound)
    }
    fn overlaps(&self, other: &Self) -> bool {
        self.model == other.model
            && self.provider == other.provider
            && self.tier == other.tier
            && self.from.unwrap_or(NaiveDate::MIN) <= other.until.unwrap_or(NaiveDate::MAX)
            && other.from.unwrap_or(NaiveDate::MIN) <= self.until.unwrap_or(NaiveDate::MAX)
            && self.min_input.unwrap_or(0) <= other.max_input.unwrap_or(u64::MAX)
            && other.min_input.unwrap_or(0) <= self.max_input.unwrap_or(u64::MAX)
    }
}

impl Table {
    fn parse(raw: &str, label: &str) -> Result<Self> {
        let mut table: Self =
            toml::from_str(raw).with_context(|| format!("invalid pricing table {label}"))?;
        let mut ids = BTreeSet::new();
        for (index, rate) in table.rates.iter_mut().enumerate() {
            rate.tier = tier(&rate.tier).to_owned();
            if rate.id.is_empty() {
                rate.id = format!("{label}:{}:{}:{index}", rate.model, rate.tier);
            }
            if !ids.insert(rate.id.clone()) {
                bail!("duplicate pricing ID {}", rate.id);
            }
            if rate.model.trim().is_empty() || rate.provider.trim().is_empty() {
                bail!("pricing model and provider must not be empty");
            }
            if !["standard", "fast", "flex", "ultrafast", "batch"].contains(&rate.tier.as_str()) {
                bail!("unsupported pricing tier {}", rate.tier);
            }
            if rate.input < Decimal::ZERO
                || rate.output < Decimal::ZERO
                || [rate.cached, rate.cache_write, rate.cache_write_12h]
                    .iter()
                    .flatten()
                    .any(|v| *v < Decimal::ZERO)
            {
                bail!("negative rate in {}", rate.id);
            }
            if rate
                .from
                .zip(rate.until)
                .is_some_and(|(start, end)| start > end)
                || rate
                    .min_input
                    .zip(rate.max_input)
                    .is_some_and(|(start, end)| start > end)
            {
                bail!("reversed date/context range in {}", rate.id);
            }
            if rate
                .cache_write_mode
                .as_deref()
                .is_some_and(|mode| !["subset", "additive"].contains(&mode))
            {
                bail!("cache_write_mode must be subset or additive in {}", rate.id);
            }
        }
        for (i, rate) in table.rates.iter().enumerate() {
            if let Some(other) = table
                .rates
                .iter()
                .skip(i + 1)
                .find(|other| rate.overlaps(other))
            {
                bail!(
                    "ambiguous overlapping pricing entries {} and {}",
                    rate.id,
                    other.id
                );
            }
        }
        Ok(table)
    }
}

impl Pricer {
    /// Estimate all usage at the current bundled snapshot, regardless of event date.
    pub fn load_current(path: Option<&Path>) -> Result<Self> {
        let mut pricer = Self::load_with_mode(path, None, true)?;
        pricer
            .bundled
            .verified
            .context("current pricing bundle has no verification date")?;
        pricer.current_snapshot = true;
        Ok(pricer)
    }

    /// Retain exact historical matching for callers of the original library API.
    pub fn load(path: Option<&Path>, repricing: Option<NaiveDate>) -> Result<Self> {
        Self::load_with_mode(path, repricing, true)
    }

    /// Allow a disclosed latest-bundle estimate when no dated rate is known.
    pub fn load_with_mode(
        path: Option<&Path>,
        repricing: Option<NaiveDate>,
        historical_only: bool,
    ) -> Result<Self> {
        let bundled = Table::parse(BUNDLE, "bundled")?;
        let (custom, custom_hash) = if let Some(path) = path {
            let raw = fs::read_to_string(path)
                .with_context(|| format!("cannot read pricing file {}", path.display()))?;
            (
                Some(Table::parse(&raw, "custom")?),
                Some(blake3::hash(raw.as_bytes()).to_hex().to_string()),
            )
        } else {
            (None, None)
        };
        Ok(Self {
            bundled,
            custom,
            custom_path: path.map(|p| p.display().to_string()),
            custom_hash,
            repricing,
            historical_only,
            current_snapshot: false,
        })
    }

    pub fn metadata(&self) -> Value {
        let models: BTreeSet<_> = self
            .custom
            .iter()
            .flat_map(|t| &t.rates)
            .chain(self.bundled.rates.iter())
            .map(|r| r.model.as_str())
            .collect();
        json!({
            "version": self.bundled.version,
            "verified": self.bundled.verified,
            "source": self.bundled.source,
            "bundle_hash": blake3::hash(BUNDLE.as_bytes()).to_hex().to_string(),
            "custom_path": self.custom_path,
            "custom_hash": self.custom_hash,
            "repricing_date": self.repricing,
            "pricing_basis": if self.current_snapshot { "current_snapshot" } else if self.repricing.is_some() { "explicit_date" } else if self.historical_only { "historical" } else { "latest_verified_fallback" },
            "historical_pricing": self.historical_only && !self.current_snapshot,
            "fallback_mode": if self.current_snapshot || self.historical_only || self.repricing.is_some() { "none" } else { "latest_verified_bundle" },
            "scope": "API-equivalent model tokens; excludes tool fees, regional uplifts, taxes and subscription charges",
            "date_bounds": "inclusive",
            "models": models,
            "rates": self.custom.iter().flat_map(|t| &t.rates).chain(self.bundled.rates.iter()).collect::<Vec<_>>(),
        })
    }

    pub fn estimate(&self, event: &UsageEvent) -> Estimate {
        let mut assumptions = Vec::new();
        if let Err(error) = event.usage.validate() {
            return Estimate::unpriced(error.to_string(), assumptions, None);
        }
        let tier_name = match event.tier.as_deref() {
            Some(value) => tier(value),
            None => {
                assumptions.push("Standard tier assumed: service tier not recorded".into());
                "standard"
            }
        };
        let date = if self.current_snapshot {
            self.bundled.verified
        } else {
            self.repricing
        }
        .unwrap_or_else(|| event.timestamp.date_naive());
        if self.repricing.is_some() {
            assumptions.push(format!(
                "Repricing scenario: {date} rates applied to all events"
            ));
        }
        let mut rate = self
            .custom
            .iter()
            .flat_map(|t| &t.rates)
            .find(|r| r.matches(event, date, tier_name))
            .or_else(|| {
                self.bundled
                    .rates
                    .iter()
                    .find(|r| r.matches(event, date, tier_name))
            });
        if rate.is_none()
            && !self.current_snapshot
            && !self.historical_only
            && self.repricing.is_none()
        {
            if let Some(verified) = self.bundled.verified {
                rate = self
                    .bundled
                    .rates
                    .iter()
                    .find(|r| r.matches(event, verified, tier_name));
                if rate.is_some() {
                    assumptions.push(format!(
                        "Estimated using rates verified on {verified}; historical price not recorded"
                    ));
                }
            }
        }
        let Some(rate) = rate else {
            return Estimate::unpriced(
                format!(
                    "No verified rate for {}/{}/{} on {}",
                    event.provider, event.model, tier_name, date
                ),
                assumptions,
                None,
            );
        };
        let id = Some(rate.id.clone());
        let cached = match event.usage.cached {
            Some(value) => value,
            None => {
                assumptions.push("Cache-read count unavailable: assumes zero cached input".into());
                0
            }
        };
        let writes = match event.usage.cache_write {
            Some(value) => value,
            None => {
                if rate.cache_write.is_some() || rate.cache_write_12h.is_some() {
                    assumptions.push(
                        "Cache-write count unavailable: assumes no cache-write charge".into(),
                    );
                }
                0
            }
        };
        let Some(mut ordinary) = event.usage.input.checked_sub(cached) else {
            return Estimate::unpriced("Cached input exceeds input", assumptions, id);
        };
        let mut write_rate = Decimal::ZERO;
        if writes > 0 {
            match rate.cache_write_mode.as_deref() {
                Some("subset") => {
                    let Some(value) = ordinary.checked_sub(writes) else {
                        return Estimate::unpriced(
                            "Cache-read and write subsets exceed input",
                            assumptions,
                            id,
                        );
                    };
                    ordinary = value;
                }
                Some("additive") => {}
                _ => {
                    return Estimate::unpriced(
                        "Cache-write subset/additive semantics are not verified for this rate",
                        assumptions,
                        id,
                    )
                }
            }
            let retention = match event.cache_retention.as_deref() {
                Some("30m") | Some("30min") => "30m",
                Some("12h") => "12h",
                Some(other) => {
                    return Estimate::unpriced(
                        format!("No cache-write rate for retention {other}"),
                        assumptions,
                        id,
                    )
                }
                None if rate.cache_write_12h.is_none() => "30m",
                None => {
                    assumptions.push("Cache retention unavailable: assumes 30m writes".into());
                    "30m"
                }
            };
            let selected = if retention == "12h" {
                rate.cache_write_12h
            } else {
                rate.cache_write
            };
            let Some(selected) = selected else {
                return Estimate::unpriced("Cache-write rate unavailable", assumptions, id);
            };
            write_rate = selected;
        }
        let cached_rate = match (cached, rate.cached) {
            (0, _) => Decimal::ZERO,
            (_, Some(value)) => value,
            _ => return Estimate::unpriced("Cached-input rate unavailable", assumptions, id),
        };
        let cost = Decimal::from(ordinary)
            .checked_mul(rate.input)
            .and_then(|v| {
                Decimal::from(cached)
                    .checked_mul(cached_rate)
                    .and_then(|c| v.checked_add(c))
            })
            .and_then(|v| {
                Decimal::from(writes)
                    .checked_mul(write_rate)
                    .and_then(|c| v.checked_add(c))
            })
            .and_then(|v| {
                Decimal::from(event.usage.output)
                    .checked_mul(rate.output)
                    .and_then(|c| v.checked_add(c))
            })
            .and_then(|v| v.checked_div(Decimal::from(1_000_000)));
        match cost {
            Some(value) => Estimate {
                cost: Some(value.normalize()),
                status: if assumptions.is_empty() {
                    "priced"
                } else {
                    "assumed"
                }
                .into(),
                assumptions,
                reason: None,
                rate_id: id,
            },
            None => Estimate::unpriced("Decimal arithmetic overflow", assumptions, id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Usage;
    use std::io::Write;
    fn event() -> UsageEvent {
        UsageEvent {
            id: "response-1".into(),
            timestamp: "2026-11-04T10:00:00Z".parse().unwrap(),
            session: "test-session".into(),
            model: "gpt-6.1-sol".into(),
            provider: "openai".into(),
            tier: Some("standard".into()),
            project: None,
            usage: Usage {
                input: 100_000,
                cached: Some(60_000),
                cache_write: Some(0),
                output: 20_000,
                reasoning: Some(12_000),
            },
            source: "test".into(),
            origins: BTreeSet::new(),
            context_tokens: None,
            cache_retention: None,
        }
    }
    fn custom(raw: &str) -> Pricer {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(raw.as_bytes()).unwrap();
        Pricer::load(Some(file.path()), None).unwrap()
    }
    #[test]
    fn ordinary_cache_reasoning_are_charged_once() {
        let estimate = Pricer::load(None, None).unwrap().estimate(&event());
        assert_eq!(estimate.cost.unwrap().to_string(), "0.286");
        assert_eq!(estimate.status, "priced");
        assert!(estimate.assumptions.is_empty());
        assert!(serde_json::to_string(&estimate)
            .unwrap()
            .contains("\"cost\":\"0.286\""));
    }
    #[test]
    fn current_snapshot_prices_all_event_dates_without_history_assumptions() {
        let p = Pricer::load_current(None).unwrap();
        let mut e = event();
        for date in ["2025-01-01", "2026-11-04", "2027-12-31"] {
            e.timestamp = format!("{date}T12:00:00Z").parse().unwrap();
            for (tier_name, expected) in [("standard", "0.286"), ("priority", "0.572")] {
                e.tier = Some(tier_name.into());
                let estimate = p.estimate(&e);
                assert_eq!(estimate.cost.unwrap().to_string(), expected);
                assert_eq!(estimate.status, "priced");
                assert!(estimate.assumptions.is_empty());
            }
        }
        assert_eq!(p.metadata()["pricing_basis"], "current_snapshot");
        assert_eq!(p.metadata()["historical_pricing"], false);
        assert_eq!(p.metadata()["fallback_mode"], "none");
        e.tier = None;
        assert!(p
            .estimate(&e)
            .assumptions
            .iter()
            .any(|a| a.contains("Standard tier assumed")));
    }
    #[test]
    fn current_snapshot_uses_custom_rates_at_snapshot_not_event_date() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"[[rates]]\nmodel='gpt-6.1-sol'\nfrom='2026-10-01'\nuntil='2026-10-31'\ninput='99'\ncached='99'\noutput='99'\n\n[[rates]]\nmodel='gpt-6.1-sol'\nfrom='2026-11-04'\nuntil='2026-11-04'\ninput='3'\ncached='1'\noutput='10'\n").unwrap();
        let p = Pricer::load_current(Some(file.path())).unwrap();
        let mut e = event();
        for date in ["2026-10-15", "2027-01-01"] {
            e.timestamp = format!("{date}T12:00:00Z").parse().unwrap();
            let estimate = p.estimate(&e);
            assert_eq!(estimate.cost.unwrap().to_string(), "0.38");
            assert_eq!(estimate.status, "priced");
            assert!(estimate.rate_id.unwrap().starts_with("custom:"));
        }
        let mut expired = tempfile::NamedTempFile::new().unwrap();
        expired.write_all(b"[[rates]]\nmodel='gpt-6.1-sol'\nfrom='2026-10-01'\nuntil='2026-10-31'\ninput='99'\noutput='99'\n").unwrap();
        let p = Pricer::load_current(Some(expired.path())).unwrap();
        e.timestamp = "2026-10-15T12:00:00Z".parse().unwrap();
        assert_eq!(p.estimate(&e).cost.unwrap().to_string(), "0.286");
        assert!(p
            .estimate(&e)
            .rate_id
            .unwrap()
            .starts_with("openai-2026-11-04:"));
    }
    #[test]
    fn request_context_and_service_tier_select_distinct_rates() {
        let p = Pricer::load(None, None).unwrap();
        let mut e = event();
        e.usage.input = 272_000;
        assert!(p.estimate(&e).rate_id.unwrap().ends_with(":short"));
        e.usage.input += 1;
        assert!(p.estimate(&e).rate_id.unwrap().ends_with(":long"));
        e.tier = Some("priority".into());
        assert!(p.estimate(&e).rate_id.unwrap().contains(":fast:"));
        e.tier = Some("imaginary".into());
        assert!(p.estimate(&e).cost.is_none());
    }
    #[test]
    fn exact_short_long_and_fast_costs_keep_cache_and_reasoning_as_subsets() {
        let p = Pricer::load_current(None).unwrap();
        let mut e = event();
        // Official rates: short Standard 2/0.1/10, long 4/0.2/15;
        // Fast doubles these. Each case has 60k cached input and 20k output.
        for (input, service_tier, expected) in [
            (272_000, "standard", "0.63"),
            (272_001, "standard", "1.160004"),
            (272_000, "priority", "1.26"),
            (272_001, "fast", "2.320008"),
        ] {
            e.usage.input = input;
            e.tier = Some(service_tier.into());
            for reasoning in [0, 12_000, 20_000] {
                e.usage.reasoning = Some(reasoning);
                assert_eq!(p.estimate(&e).cost.unwrap().to_string(), expected);
            }
        }
        e = event();
        e.usage.cached = Some(e.usage.input);
        // All input at the cache-read rate, with output charged exactly once.
        assert_eq!(p.estimate(&e).cost.unwrap().to_string(), "0.21");
        e.usage.cached = None;
        let assumed = p.estimate(&e);
        assert_eq!(assumed.cost.unwrap().to_string(), "0.4");
        assert_eq!(assumed.status, "assumed");
        assert!(assumed
            .assumptions
            .iter()
            .any(|a| a.contains("zero cached input")));
    }

    #[test]
    fn write_retention_and_missing_cache_rates_do_not_produce_silent_discounts() {
        let p = custom("[[rates]]\nmodel='gpt-6.1-sol'\ninput='2'\ncached='0.1'\noutput='10'\ncache_write='2.5'\ncache_write_12h='5'\ncache_write_mode='subset'\n");
        let mut e = event();
        e.usage.cache_write = Some(10_000);
        e.cache_retention = Some("12h".into());
        // 30k ordinary*2 + 60k cached*0.1 + 10k writes*5 + 20k output*10.
        assert_eq!(p.estimate(&e).cost.unwrap().to_string(), "0.316");
        e.cache_retention = Some("unsupported".into());
        assert!(p.estimate(&e).cost.is_none());
        let p = Pricer::load_current(None).unwrap();
        e = event();
        e.model = "gpt-5.5-pro".into();
        assert!(p.estimate(&e).cost.is_none());
        e.usage.cached = Some(0);
        assert_eq!(p.estimate(&e).cost.unwrap().to_string(), "6.6");
    }
    #[test]
    fn missing_tier_is_marked_and_past_rates_are_not_invented() {
        let mut e = event();
        e.tier = None;
        assert_eq!(
            Pricer::load(None, None).unwrap().estimate(&e).status,
            "assumed"
        );
        e.timestamp = "2026-11-03T12:00:00Z".parse().unwrap();
        assert!(Pricer::load(None, None)
            .unwrap()
            .estimate(&e)
            .cost
            .is_none());
        let p = Pricer::load(None, Some("2026-11-04".parse().unwrap())).unwrap();
        assert!(p.estimate(&e).cost.is_some());
        assert!(p
            .estimate(&e)
            .assumptions
            .iter()
            .any(|a| a.contains("Repricing")));
    }
    #[test]
    fn latest_bundle_fallback_estimates_regular_and_fast_history() {
        let p = Pricer::load_with_mode(None, None, false).unwrap();
        assert_eq!(p.estimate(&event()).status, "priced");
        let mut e = event();
        e.timestamp = "2026-10-01T12:00:00Z".parse().unwrap();
        let estimate = p.estimate(&e);
        assert_eq!(estimate.cost.unwrap().to_string(), "0.286");
        assert_eq!(estimate.status, "assumed");
        assert!(estimate
            .assumptions
            .iter()
            .any(|a| a.contains("rates verified on 2026-11-04; historical price not recorded")));
        e.tier = Some("priority".into());
        assert_eq!(p.estimate(&e).cost.unwrap().to_string(), "0.572");
        assert!(p.estimate(&e).rate_id.unwrap().contains(":fast:"));
        e.usage.input = 272_001;
        assert!(p.estimate(&e).rate_id.unwrap().ends_with(":long"));
        assert_eq!(p.metadata()["historical_pricing"], false);
        assert_eq!(p.metadata()["fallback_mode"], "latest_verified_bundle");
        e.model = "unknown-alias".into();
        assert!(p.estimate(&e).cost.is_none());
        e = event();
        e.provider = "other".into();
        assert!(p.estimate(&e).cost.is_none());
        e = event();
        e.timestamp = "2026-10-01T12:00:00Z".parse().unwrap();
        e.usage.cache_write = Some(10_000);
        assert!(p.estimate(&e).cost.is_none());
    }
    #[test]
    fn fallback_never_uses_custom_rates_outside_their_dates() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"[[rates]]\nmodel='gpt-6.1-sol'\nfrom='2026-11-02'\nuntil='2026-11-03'\ninput='5'\ncached='1'\noutput='1'\n").unwrap();
        let p = Pricer::load_with_mode(Some(file.path()), None, false).unwrap();
        let mut e = event();
        for date in ["2026-11-02", "2026-11-03"] {
            e.timestamp = format!("{date}T12:00:00Z").parse().unwrap();
            let estimate = p.estimate(&e);
            assert_eq!(estimate.cost.unwrap().to_string(), "0.28");
            assert!(estimate.rate_id.unwrap().starts_with("custom:"));
            assert_eq!(estimate.status, "priced");
        }
        for date in ["2026-11-01", "2026-11-05"] {
            e.timestamp = format!("{date}T12:00:00Z").parse().unwrap();
            let estimate = p.estimate(&e);
            assert_eq!(estimate.cost.unwrap().to_string(), "0.286");
            assert!(estimate.rate_id.unwrap().starts_with("openai-2026-11-04:"));
            assert_eq!(estimate.status, "assumed");
        }
    }
    #[test]
    fn explicit_repricing_and_historical_mode_disable_fallback() {
        let mut e = event();
        e.timestamp = "2026-11-01T12:00:00Z".parse().unwrap();
        let historical = Pricer::load_with_mode(None, None, true).unwrap();
        assert!(historical.estimate(&e).cost.is_none());
        assert_eq!(historical.metadata()["fallback_mode"], "none");
        let repricing =
            Pricer::load_with_mode(None, Some("2026-11-03".parse().unwrap()), false).unwrap();
        assert!(repricing.estimate(&e).cost.is_none());
        assert_eq!(repricing.metadata()["fallback_mode"], "none");
        let repricing =
            Pricer::load_with_mode(None, Some("2026-11-04".parse().unwrap()), false).unwrap();
        assert_eq!(repricing.estimate(&e).cost.unwrap().to_string(), "0.286");
        assert!(repricing
            .estimate(&e)
            .assumptions
            .iter()
            .any(|a| a.contains("Repricing scenario")));
        assert!(!repricing
            .estimate(&e)
            .assumptions
            .iter()
            .any(|a| a.contains("historical price not recorded")));
    }
    #[test]
    fn unknown_backend_alias_or_provider_remains_unpriced() {
        let p = Pricer::load(None, None).unwrap();
        let mut e = event();
        e.model = "codex-auto-review".into();
        assert!(p.estimate(&e).cost.is_none());
        e = event();
        e.provider = "other".into();
        assert!(p.estimate(&e).cost.is_none());
    }
    #[test]
    fn custom_overrides_matching_scope_and_falls_back_for_other_tiers() {
        let p = custom("[[rates]]\nmodel='gpt-6.1-sol'\ninput='3'\ncached='1'\noutput='10'\n");
        let mut e = event();
        assert_eq!(p.estimate(&e).cost.unwrap().to_string(), "0.38");
        e.tier = Some("fast".into());
        assert_eq!(p.estimate(&e).cost.unwrap().to_string(), "0.572");
    }
    #[test]
    fn cache_writes_require_explicit_verified_semantics() {
        let mut e = event();
        e.usage.cache_write = Some(10_000);
        assert!(Pricer::load(None, None)
            .unwrap()
            .estimate(&e)
            .cost
            .is_none());
        let p=custom("[[rates]]\nmodel='gpt-6.1-sol'\ninput='2'\ncached='0.1'\noutput='10'\ncache_write='2.5'\ncache_write_mode='subset'\n");
        assert_eq!(p.estimate(&e).cost.unwrap().to_string(), "0.291");
        let p=custom("[[rates]]\nmodel='gpt-6.1-sol'\ninput='2'\ncached='0.1'\noutput='10'\ncache_write='0.5'\ncache_write_mode='additive'\n");
        assert_eq!(p.estimate(&e).cost.unwrap().to_string(), "0.291");
        e.usage.cache_write = Some(50_000);
        let p=custom("[[rates]]\nmodel='gpt-6.1-sol'\ninput='2'\ncached='0.1'\noutput='10'\ncache_write='2.5'\ncache_write_mode='subset'\n");
        assert!(p.estimate(&e).cost.is_none());
    }
    #[test]
    fn custom_validation_rejects_negative_and_ambiguous_ranges() {
        assert!(Table::parse("[[rates]]\nmodel='x'\ninput='-1'\noutput='2'\n", "custom").is_err());
        let raw="[[rates]]\nmodel='x'\ninput='1'\noutput='2'\n[[rates]]\nmodel='x'\ninput='3'\noutput='4'\n";
        assert!(Table::parse(raw, "custom").is_err());
        let raw="[[rates]]\nmodel='x'\nfrom='2026-01-01'\nuntil='2026-01-31'\ninput='1'\noutput='2'\n[[rates]]\nmodel='x'\nfrom='2026-02-01'\ninput='3'\noutput='4'\n";
        assert!(Table::parse(raw, "custom").is_ok());
    }
}
