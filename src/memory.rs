use crate::config::Workspace;
use anyhow::Result;

const HEADER: &str =
    "<!-- AUTO-GENERATED FROM blocks/*.md BEFORE EACH PI INVOCATION. DO NOT EDIT BY HAND. -->\n";

pub fn render_agents_md(ws: &Workspace) -> Result<()> {
    let mut buf = String::from(HEADER);
    buf.push_str("# Glass — agent context\n\n");
    buf.push_str(
        "This file is rendered fresh from `blocks/*.md` before every Pi invocation. \
         These notes are always in the prompt. Longer notes live in `state/` — read them on demand.\n",
    );

    let blocks_dir = ws.blocks_dir();
    let mut entries: Vec<_> = std::fs::read_dir(&blocks_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("md"))
        .collect();
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let path = entry.path();
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("block");
        let body = std::fs::read_to_string(&path)?;
        buf.push_str(&format!("\n---\n## block: {stem}\n\n"));
        buf.push_str(body.trim());
        buf.push('\n');
    }

    buf.push_str("\n---\n## memory tools\n\n");
    buf.push_str(
        "- Identity / style / current focus / relationships: edit `blocks/<name>.md` \
         (it'll appear in your prompt next turn).\n",
    );
    buf.push_str(
        "- Longer notes (project tracking, research, dossiers, anything bulkier): write to \
         `state/<topic>.md`. You can `read` them when relevant; they aren't auto-loaded.\n",
    );
    buf.push_str(
        "- Conversation history: `history/events.jsonl` (audit, append-only) and \
         `history/current.jsonl` (live conversation, rewritten on compaction). \
         Both are written by the bot — don't edit either.\n",
    );
    buf.push_str(
        "- Keep blocks short. If a block grows past ~30 lines, move the bulk into `state/` and \
         leave a pointer in the block.\n",
    );

    std::fs::write(ws.agents_md(), buf)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn ws_with_blocks(entries: &[(&str, &str)]) -> (TempDir, Workspace) {
        let tmp = TempDir::new().unwrap();
        let ws = Workspace {
            root: tmp.path().to_path_buf(),
        };
        ws.ensure_layout().unwrap();
        for (name, body) in entries {
            std::fs::write(ws.blocks_dir().join(name), body).unwrap();
        }
        (tmp, ws)
    }

    #[test]
    fn renders_blocks_in_filename_sorted_order() {
        let (_tmp, ws) = ws_with_blocks(&[
            ("z_focus.md", "# focus body"),
            ("a_identity.md", "# identity body"),
            ("m_style.md", "# style body"),
        ]);
        render_agents_md(&ws).unwrap();
        let out = std::fs::read_to_string(ws.agents_md()).unwrap();
        let i = out.find("identity body").unwrap();
        let s = out.find("style body").unwrap();
        let f = out.find("focus body").unwrap();
        assert!(i < s && s < f);
    }

    #[test]
    fn references_correct_history_filenames() {
        let (_tmp, ws) = ws_with_blocks(&[("identity.md", "# Identity\nGlass.")]);
        render_agents_md(&ws).unwrap();
        let out = std::fs::read_to_string(ws.agents_md()).unwrap();
        assert!(out.contains("events.jsonl"));
        assert!(out.contains("current.jsonl"));
        assert!(!out.contains("dm.jsonl"));
    }

    #[test]
    fn second_render_overwrites_first() {
        let (_tmp, ws) = ws_with_blocks(&[("identity.md", "first version")]);
        render_agents_md(&ws).unwrap();
        std::fs::write(ws.blocks_dir().join("identity.md"), "second version").unwrap();
        render_agents_md(&ws).unwrap();
        let out = std::fs::read_to_string(ws.agents_md()).unwrap();
        assert!(out.contains("second version"));
        assert!(!out.contains("first version"));
    }
}
