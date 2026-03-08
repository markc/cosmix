use anyhow::{Context, Result};

pub struct MidiPort {
    pub name: String,
}

pub fn list_ports() -> Result<(Vec<MidiPort>, Vec<MidiPort>)> {
    let output = std::process::Command::new("pw-link")
        .args(["-o"])
        .output()
        .context("Failed to run pw-link -o")?;
    let out_text = String::from_utf8_lossy(&output.stdout);
    let outputs: Vec<MidiPort> = out_text.lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && l.to_lowercase().contains("midi"))
        .map(|l| MidiPort { name: l.to_string() })
        .collect();

    let output = std::process::Command::new("pw-link")
        .args(["-i"])
        .output()
        .context("Failed to run pw-link -i")?;
    let in_text = String::from_utf8_lossy(&output.stdout);
    let inputs: Vec<MidiPort> = in_text.lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && l.to_lowercase().contains("midi"))
        .map(|l| MidiPort { name: l.to_string() })
        .collect();

    Ok((outputs, inputs))
}

pub fn list_connections() -> Result<Vec<(String, String)>> {
    let output = std::process::Command::new("pw-link")
        .args(["-l"])
        .output()
        .context("Failed to run pw-link -l")?;
    let text = String::from_utf8_lossy(&output.stdout);

    let mut connections = Vec::new();
    let mut current_output: Option<String> = None;

    for line in text.lines() {
        if !line.starts_with(' ') && !line.starts_with('\t') && !line.is_empty() {
            current_output = Some(line.trim().to_string());
        } else if line.contains("|->") || line.contains("-> ") {
            if let Some(ref out) = current_output {
                let input = line.trim().trim_start_matches("|-> ").trim_start_matches("-> ").trim();
                if out.to_lowercase().contains("midi") || input.to_lowercase().contains("midi") {
                    connections.push((out.clone(), input.to_string()));
                }
            }
        }
    }

    Ok(connections)
}

pub fn connect(output: &str, input: &str) -> Result<()> {
    let status = std::process::Command::new("pw-link")
        .args([output, input])
        .status()
        .context("Failed to run pw-link")?;

    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("pw-link failed to connect {output} -> {input}")
    }
}

pub fn disconnect(output: &str, input: &str) -> Result<()> {
    let status = std::process::Command::new("pw-link")
        .args(["-d", output, input])
        .status()
        .context("Failed to run pw-link -d")?;

    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("pw-link failed to disconnect {output} -> {input}")
    }
}
