use crate::core::detect::RepeatGroup;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

#[derive(Debug, Serialize, Deserialize)]
pub struct JumpPoint {
    pub time: f64,
    pub kind: String,
    pub instance: String,
    pub target: f64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InstanceOut {
    pub id: String,
    pub start: f64,
    pub end: f64,
    pub duration: f64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GroupOut {
    pub group_id: usize,
    pub instances: Vec<InstanceOut>,
    pub jump_points: Vec<JumpPoint>,
    pub confidence: f32,
    pub match_type: String,
    pub offset_sec: f64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AnalysisOut {
    pub source_file: String,
    pub total_duration_sec: f64,
    pub groups: Vec<GroupOut>,
}

pub fn build(groups: &[RepeatGroup], source: &str, total_duration: f64) -> AnalysisOut {
    let groups = groups
        .iter()
        .map(|g| {
            let (sa, ea) = g.a;
            let (sb, eb) = g.b;
            GroupOut {
                group_id: g.group_id,
                instances: vec![
                    InstanceOut {
                        id: "A".into(),
                        start: sa,
                        end: ea,
                        duration: ea - sa,
                    },
                    InstanceOut {
                        id: "B".into(),
                        start: sb,
                        end: eb,
                        duration: eb - sb,
                    },
                ],
                jump_points: vec![
                    JumpPoint {
                        time: sa,
                        kind: "start".into(),
                        instance: "A".into(),
                        target: sb,
                    },
                    JumpPoint {
                        time: ea,
                        kind: "end".into(),
                        instance: "A".into(),
                        target: eb,
                    },
                    JumpPoint {
                        time: sb,
                        kind: "start".into(),
                        instance: "B".into(),
                        target: sa,
                    },
                    JumpPoint {
                        time: eb,
                        kind: "end".into(),
                        instance: "B".into(),
                        target: ea,
                    },
                ],
                confidence: g.confidence,
                match_type: g.match_type.to_string(),
                offset_sec: g.offset_sec,
            }
        })
        .collect();

    AnalysisOut {
        source_file: source.to_string(),
        total_duration_sec: total_duration,
        groups,
    }
}

pub fn write_json(o: &AnalysisOut, p: &Path) -> Result<()> {
    fs::write(p, serde_json::to_string_pretty(o)?)?;
    Ok(())
}

pub fn write_csv(o: &AnalysisOut, p: &Path) -> Result<()> {
    use std::io::Write;
    let mut file = std::fs::File::create(p)?;
    file.write_all(b"\xEF\xBB\xBF")?;
    let mut w = csv::Writer::from_writer(file);
    w.write_record([
        "group_id",
        "instance",
        "start_sec",
        "end_sec",
        "duration_sec",
        "confidence",
        "match_type",
    ])?;
    for g in &o.groups {
        for inst in &g.instances {
            w.write_record([
                g.group_id.to_string(),
                inst.id.clone(),
                format!("{:.1}", inst.start),
                format!("{:.1}", inst.end),
                format!("{:.1}", inst.duration),
                format!("{:.3}", g.confidence),
                g.match_type.clone(),
            ])?;
        }
    }
    w.flush()?;
    Ok(())
}

pub fn write_labels(o: &AnalysisOut, p: &Path) -> Result<()> {
    let mut s = String::new();
    for g in &o.groups {
        for inst in &g.instances {
            s.push_str(&format!(
                "{:.6}\t{:.6}\tT{} {}\n",
                inst.start, inst.end, g.group_id, inst.id
            ));
        }
    }
    fs::write(p, s)?;
    Ok(())
}
