use crate::report::Report;
use anyhow::Result;
use std::io::Write;

/// Serialize retained text directly, without copying the transcript into a JSON value.
pub fn conversation_json(
    conversation: &crate::conversation::Conversation,
    usage: &Report,
    mut out: impl Write,
) -> Result<()> {
    #[derive(serde::Serialize)]
    struct Envelope<'a> {
        schema_version: u32,
        conversation: &'a crate::conversation::Conversation,
        usage: &'a Report,
    }
    serde_json::to_writer_pretty(
        &mut out,
        &Envelope {
            schema_version: 2,
            conversation,
            usage,
        },
    )?;
    writeln!(out)?;
    out.flush()?;
    Ok(())
}

pub fn json(report: &Report, mut out: impl Write, lines: bool) -> Result<()> {
    if lines {
        serde_json::to_writer(&mut out, report)?;
    } else {
        serde_json::to_writer_pretty(&mut out, report)?;
    }
    writeln!(out)?;
    out.flush()?;
    Ok(())
}
pub fn csv(report: &Report, out: impl Write) -> Result<()> {
    let mut w = csv::Writer::from_writer(out);
    w.write_record([
        "schema_version",
        "agent",
        "provider",
        "day",
        "month",
        "source",
        "session",
        "project",
        "model",
        "mode",
        "agents",
        "providers",
        "input",
        "cached",
        "cache_write",
        "output",
        "reasoning",
        "total",
        "sessions",
        "cost_usd",
        "priced_tokens",
        "unpriced_tokens",
        "cached_missing",
        "cache_write_missing",
        "reasoning_missing",
        "assumed_events",
        "pricing_status",
    ])?;
    for row in &report.rows {
        let c = &row.counts;
        let mut values = ["agent", "provider"]
            .into_iter()
            .map(|key| row.dimensions.get(key).cloned().unwrap_or_default())
            .collect::<Vec<_>>();
        values.insert(0, "2".into());
        values.extend(
            [
                "day", "month", "source", "session", "project", "model", "mode",
            ]
            .into_iter()
            .map(|key| row.dimensions.get(key).cloned().unwrap_or_default()),
        );
        if values[8].is_empty() {
            values[8] = c.modes.iter().cloned().collect::<Vec<_>>().join("|");
        }
        values.extend([
            serde_json::to_string(&c.agents)?,
            serde_json::to_string(&c.providers)?,
        ]);
        values.extend([
            c.input.to_string(),
            c.cached.to_string(),
            c.cache_write.to_string(),
            c.output.to_string(),
            c.reasoning.to_string(),
            c.total.to_string(),
            c.sessions.to_string(),
            c.cost_usd
                .map(|d| d.normalize().to_string())
                .unwrap_or_default(),
            c.priced_tokens.to_string(),
            c.unpriced_tokens.to_string(),
            c.cached_missing.to_string(),
            c.cache_write_missing.to_string(),
            c.reasoning_missing.to_string(),
            c.assumed_events.to_string(),
            c.pricing_status.clone(),
        ]);
        w.write_record(values)?;
    }
    w.flush()?;
    Ok(())
}
