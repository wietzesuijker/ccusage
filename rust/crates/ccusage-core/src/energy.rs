//! Opt-in energy and carbon estimate from cost, using CLEER per-model Wh-per-USD.
//!
//! Source: CLEER dashboard data (Sustainable AI Group, <https://cleerdash.sustainableaigroup.com/>), CC BY-NC 4.0,
//! so it is read from a user-supplied CSV path and never bundled. Only Agentic/Typical rows are used:
//! Wh = cost_usd x energy_wh_per_usd, band = +/- the row's energy SD/mean, and gCO2e scales with the row's
//! carbon/energy ratio unless a grid intensity override is set. Chat rows run ~2.7x higher in Wh/USD, so the
//! estimate is only valid for agentic sessions.

use std::{collections::HashMap, ops::AddAssign, path::Path};

use crate::cli::environment::Environment;

const CSV_ENV: &str = "CCUSAGE_ENERGY_CSV";
const GRID_ENV: &str = "CCUSAGE_GRID_G_PER_KWH";
const ENERGY_ONLY_ENV: &str = "CCUSAGE_ENERGY_ONLY";
const FAMILIES: [&str; 4] = ["opus", "sonnet", "haiku", "fable"];

#[derive(Debug, Clone, PartialEq)]
pub struct EnergyConfig {
    pub csv_path: String,
    pub grid_g_per_kwh: Option<f64>,
    pub carbon: bool,
}

impl EnergyConfig {
    /// `None` when `CCUSAGE_ENERGY_CSV` is unset or empty, which leaves output unchanged.
    pub fn from_env() -> Option<Self> {
        let env = Environment::current();
        let csv_path = env
            .var(CSV_ENV)
            .ok()
            .filter(|path| !path.trim().is_empty())?;
        let grid_g_per_kwh = env
            .var(GRID_ENV)
            .ok()
            .and_then(|value| value.trim().parse::<f64>().ok())
            .filter(|grid| *grid > 0.0);
        let energy_only = env
            .var(ENERGY_ONLY_ENV)
            .is_ok_and(|value| matches!(value.trim(), "1" | "true"));
        Some(Self {
            csv_path,
            grid_g_per_kwh,
            carbon: !energy_only,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Family {
    name: &'static str,
    /// Tenths, so 4.5 is 45.
    version: u32,
}

#[derive(Debug, Clone)]
struct Row {
    key: String,
    family: Option<Family>,
    wh_per_usd: f64,
    sd_ratio: f64,
    g_per_wh: f64,
}

#[derive(Debug, Default)]
pub struct EnergyTable {
    rows: Vec<Row>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Estimate {
    wh: f64,
    lo: f64,
    hi: f64,
    gco2e: f64,
    proxy: bool,
}

impl AddAssign for Estimate {
    fn add_assign(&mut self, other: Self) {
        self.wh += other.wh;
        self.lo += other.lo;
        self.hi += other.hi;
        self.gco2e += other.gco2e;
        self.proxy |= other.proxy;
    }
}

/// Quote-aware CSV line split (CLEER descriptions contain commas).
fn split_csv(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        match (quoted, ch) {
            (true, '"') if chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            (true, '"') => quoted = false,
            (false, '"') => quoted = true,
            (false, ',') => out.push(std::mem::take(&mut cur)),
            _ => cur.push(ch),
        }
    }
    out.push(cur);
    out
}

/// "claude-opus-5-5", "Opus 5.5", "Claude Sonnet 4.5" -> opus 5.5
fn parse_family(model: &str) -> Option<Family> {
    let lower = model.to_lowercase();
    let (name, at) = FAMILIES
        .iter()
        .filter_map(|name| lower.find(name).map(|at| (*name, at)))
        .min_by_key(|(_, at)| *at)?;
    let rest = lower[at + name.len()..].trim_start_matches([' ', '\t', '-']);
    let major_len = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    let major: u32 = rest[..major_len].parse().ok()?;
    let mut tail = rest[major_len..].chars();
    let minor = match (tail.next(), tail.next(), tail.next()) {
        (Some('.' | '-'), Some(d), after)
            if d.is_ascii_digit() && !after.is_some_and(|c| c.is_ascii_digit()) =>
        {
            d.to_digit(10).unwrap_or(0)
        }
        _ => 0,
    };
    Some(Family {
        name,
        version: major * 10 + minor,
    })
}

/// "claude-opus-4-5-20251101[1m]", "GPT-5.4 mini" -> "opus 4.5", "gpt 5.4 mini"
fn name_key(model: &str) -> String {
    let mut chars: Vec<char> = model.to_lowercase().chars().collect();
    // Strip `[...]` suffixes such as `[1m]`.
    let mut depth = 0usize;
    chars.retain(|&c| match c {
        '[' => {
            depth += 1;
            false
        }
        ']' if depth > 0 => {
            depth -= 1;
            false
        }
        _ => depth == 0,
    });
    // Strip `-YYYYMMDD` snapshot dates.
    let mut stripped = Vec::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        let is_date = chars[i] == '-'
            && chars
                .get(i + 1..i + 9)
                .is_some_and(|d| d.iter().all(char::is_ascii_digit))
            && !chars
                .get(i + 9)
                .is_some_and(|c| c.is_alphanumeric() || *c == '_');
        if is_date {
            i += 9;
        } else {
            stripped.push(chars[i]);
            i += 1;
        }
    }
    // "4-5" -> "4.5" when the second number is a single digit.
    let mut dotted = stripped.clone();
    for i in 1..stripped.len().saturating_sub(1) {
        if stripped[i] == '-'
            && stripped[i - 1].is_ascii_digit()
            && stripped[i + 1].is_ascii_digit()
            && !stripped.get(i + 2).is_some_and(char::is_ascii_digit)
        {
            dotted[i] = '.';
        }
    }
    let spaced: String = dotted
        .into_iter()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' {
                c
            } else {
                ' '
            }
        })
        .collect();
    let spaced = spaced.split_whitespace().collect::<Vec<_>>().join(" ");
    spaced
        .strip_prefix("claude ")
        .map_or(spaced.clone(), str::to_string)
}

impl EnergyTable {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        Self::parse(&text)
    }

    /// Keeps only Agentic/Typical rows with a positive mean energy and Wh per USD.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut lines = text.lines().filter(|line| !line.is_empty());
        let head = split_csv(lines.next().unwrap_or_default());
        let col: HashMap<&str, usize> = head
            .iter()
            .enumerate()
            .map(|(i, h)| (h.as_str(), i))
            .collect();
        let idx = |name: &str| {
            col.get(name)
                .copied()
                .ok_or_else(|| format!("CLEER CSV missing column: {name}"))
        };
        let (name_i, use_i, tier_i) = (idx("model_name")?, idx("use_case")?, idx("tier")?);
        let (mean_i, sd_i) = (idx("energy_wh_mean")?, idx("energy_wh_sd")?);
        let (carbon_i, usd_i) = (idx("carbon_gco2e_mean")?, idx("energy_wh_per_usd")?);
        let num = |row: &[String], i: usize| {
            row.get(i)
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(f64::NAN)
        };
        let mut rows = Vec::new();
        for line in lines {
            let r = split_csv(line);
            if r.get(use_i).map(String::as_str) != Some("Agentic")
                || r.get(tier_i).map(String::as_str) != Some("Typical")
            {
                continue;
            }
            let (mean, wh_per_usd) = (num(&r, mean_i), num(&r, usd_i));
            if !mean.is_finite() || mean <= 0.0 || !wh_per_usd.is_finite() || wh_per_usd <= 0.0 {
                continue;
            }
            let name = r.get(name_i).map_or("", String::as_str);
            rows.push(Row {
                key: name_key(name),
                family: parse_family(name),
                wh_per_usd,
                sd_ratio: num(&r, sd_i) / mean,
                g_per_wh: num(&r, carbon_i) / mean,
            });
        }
        Ok(Self { rows })
    }

    /// Exact name (any provider); for Claude, else the newest same-family row at or below the version, else newest of family.
    fn find(&self, model: &str) -> Option<(&Row, bool)> {
        let key = name_key(model);
        if let Some(row) = self.rows.iter().find(|row| row.key == key) {
            return Some((row, false));
        }
        let want = parse_family(model)?;
        let mut family: Vec<(&Row, u32)> = self
            .rows
            .iter()
            .filter_map(|row| {
                row.family
                    .filter(|f| f.name == want.name)
                    .map(|f| (row, f.version))
            })
            .collect();
        family.sort_by_key(|(_, version)| std::cmp::Reverse(*version));
        if let Some((row, _)) = family.iter().find(|(_, v)| *v == want.version) {
            return Some((row, false));
        }
        let near = family
            .iter()
            .find(|(_, v)| *v < want.version)
            .or(family.first())?;
        Some((near.0, true))
    }

    /// `None` for an unknown model or a non-positive cost.
    pub fn estimate(&self, model: &str, cost_usd: f64) -> Option<Estimate> {
        let (row, proxy) = self.find(model)?;
        if !cost_usd.is_finite() || cost_usd <= 0.0 {
            return None;
        }
        let wh = cost_usd * row.wh_per_usd;
        Some(Estimate {
            wh,
            lo: wh * (1.0 - row.sd_ratio).max(0.0),
            hi: wh * (1.0 + row.sd_ratio),
            gco2e: wh * row.g_per_wh,
            proxy,
        })
    }

    /// Sums per-model estimates; models without a match are skipped, and `None` means none matched.
    pub fn estimate_total<'a>(
        &self,
        costs: impl IntoIterator<Item = (&'a str, f64)>,
    ) -> Option<Estimate> {
        costs
            .into_iter()
            .filter_map(|(model, cost)| self.estimate(model, cost))
            .fold(None, |total, e| {
                let mut sum = total.unwrap_or_default();
                sum += e;
                Some(sum)
            })
    }
}

/// Shared display spec: >=10 integer, <10 one decimal, anything displaying as 0.0 shows "0".
fn num(x: f64) -> String {
    let one = format!("{x:.1}");
    if x == 0.0 || one == "0.0" {
        "0".to_string()
    } else if x >= 10.0 || one == "10.0" {
        format!("{x:.0}")
    } else {
        one
    }
}

impl Estimate {
    /// "~35-52 Wh · 22-33 gCO₂e"; "~" marks a proxied model version.
    pub fn format(&self, config: &EnergyConfig) -> String {
        let kwh = self.hi >= 1000.0;
        let unit = if kwh { "kWh" } else { "Wh" };
        let energy = |x: f64| num(if kwh { x / 1000.0 } else { x });
        let band = |lo: f64, hi: f64, mid: f64, f: &dyn Fn(f64) -> String| {
            let (a, b) = (f(lo), f(hi));
            if hi > 0.0 && b == "0" {
                "<0.1".to_string()
            } else if a == b {
                f(mid)
            } else {
                format!("{a}-{b}")
            }
        };
        let label = format!(
            "{}{} {unit}",
            if self.proxy { "~" } else { "" },
            band(self.lo, self.hi, self.wh, &energy)
        );
        if !config.carbon {
            return label;
        }
        let g = config
            .grid_g_per_kwh
            .map_or(self.gco2e / self.wh, |grid| grid / 1000.0);
        let grams = band(self.lo * g, self.hi * g, self.wh * g, &num);
        format!("{label} · {grams} gCO₂e")
    }
}

#[cfg(test)]
mod tests;
