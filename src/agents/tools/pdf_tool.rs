//! PDF text extraction tool.

use super::{AgentTool, ToolContext, ToolInfo, ToolResult};
use anyhow::Result;
use async_trait::async_trait;

pub struct PdfTool;

#[async_trait]
impl AgentTool for PdfTool {
    fn info(&self) -> ToolInfo {
        ToolInfo {
            name: "pdf_extract".to_string(),
            description: "Extract text content from a PDF file".to_string(),
            category: "document".to_string(),
            hidden: false,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the PDF file"
                    },
                    "pages": {
                        "type": "string",
                        "description": "Page range to extract (e.g. '1-5', '1,3,5-7'). Defaults to all pages."
                    },
                    "maxChars": {
                        "type": "integer",
                        "description": "Maximum characters to return",
                        "default": 50000
                    }
                },
                "required": ["path"]
            }),
        }
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _context: &ToolContext,
    ) -> Result<ToolResult> {
        let path = params
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing path parameter"))?;

        let max_chars = params
            .get("maxChars")
            .and_then(|v| v.as_u64())
            .unwrap_or(50000) as usize;

        let pages_spec = params.get("pages").and_then(|v| v.as_str());

        // Read the file
        let file_data = tokio::fs::read(path).await.map_err(|e| {
            anyhow::anyhow!("Failed to read PDF file '{}': {}", path, e)
        })?;

        // Try to extract text using a subprocess (pdftotext)
        let text = extract_with_pdftotext(path, pages_spec).await
            .or_else(|_| extract_basic(&file_data))
            .unwrap_or_else(|e| format!("Failed to extract PDF text: {}", e));

        let text = if text.len() > max_chars {
            format!(
                "{}... (truncated, {} chars total)",
                &text[..max_chars],
                text.len()
            )
        } else {
            text
        };

        let page_count = count_pages(&file_data);

        Ok(ToolResult::json(serde_json::json!({
            "text": text,
            "pages": page_count,
            "chars": text.len(),
            "path": path
        })))
    }
}

async fn extract_with_pdftotext(path: &str, pages: Option<&str>) -> Result<String> {
    let mut cmd = tokio::process::Command::new("pdftotext");

    if let Some(page_spec) = pages {
        if let Some((first, last)) = parse_page_range(page_spec) {
            cmd.arg("-f").arg(first.to_string());
            cmd.arg("-l").arg(last.to_string());
        }
    }

    cmd.arg(path).arg("-");

    let output = cmd.output().await?;

    if !output.status.success() {
        anyhow::bail!(
            "pdftotext failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn extract_basic(data: &[u8]) -> Result<String> {
    // Basic PDF text extraction by finding text streams
    let content = String::from_utf8_lossy(data);
    let mut texts = Vec::new();

    // Extract text between BT/ET markers (very basic)
    let mut in_text = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed == "BT" {
            in_text = true;
            continue;
        }
        if trimmed == "ET" {
            in_text = false;
            continue;
        }
        if in_text {
            // Extract text from Tj and TJ operators
            if let Some(text) = extract_tj_text(trimmed) {
                texts.push(text);
            }
        }
    }

    if texts.is_empty() {
        anyhow::bail!("No text found in PDF (may be image-only)");
    }

    Ok(texts.join(" "))
}

fn extract_tj_text(line: &str) -> Option<String> {
    // Match (text) Tj pattern
    if line.ends_with("Tj") || line.ends_with("TJ") {
        if let Some(start) = line.find('(') {
            if let Some(end) = line.rfind(')') {
                return Some(line[start + 1..end].to_string());
            }
        }
    }
    None
}

fn parse_page_range(spec: &str) -> Option<(u32, u32)> {
    if let Some((first, last)) = spec.split_once('-') {
        let f = first.trim().parse().ok()?;
        let l = last.trim().parse().ok()?;
        Some((f, l))
    } else {
        let page: u32 = spec.trim().parse().ok()?;
        Some((page, page))
    }
}

fn count_pages(data: &[u8]) -> u32 {
    // Count /Type /Page occurrences (rough estimate)
    let content = String::from_utf8_lossy(data);
    content.matches("/Type /Page").count().saturating_sub(
        content.matches("/Type /Pages").count()
    ) as u32
}
