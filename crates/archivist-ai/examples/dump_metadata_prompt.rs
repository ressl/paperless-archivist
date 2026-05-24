//! Render the consolidated metadata prompt with a representative payload
//! to standard output. Used during the v1.5.29 prompt redesign to eyeball
//! the final wire format. Not wired into the build by default; run with:
//!
//!   cargo run -p archivist-ai --example dump_metadata_prompt
//!
//! The output is the exact concatenated system + user prompt the worker
//! would send to the AI provider for a German invoice with all six keys
//! enabled.

use archivist_ai::{PromptLanguageContext, prompt_for_metadata, schema_for_metadata};
use archivist_core::MetadataFieldFlags;

fn main() {
    let language = PromptLanguageContext {
        document_language: "de".to_owned(),
        document_language_confidence: 0.96,
        tag_output_language: "de".to_owned(),
    };
    let request = prompt_for_metadata(
        "DITech Daten- & Informationstechnik GmbH\n\
         Wehlistraße 29, 1200 Wien\n\
         Rechnung Nr. 4091\n\
         Rechnungsdatum: 12.02.2003\n\
         Kundennummer: 38381\n\
         Herr Robert Reßl\n\
         Wienerbergstraße 22, 1100 Wien\n\
         \n\
         Leistung: Wartung Server-Cluster\n\
         Gesamtbetrag: EUR 1.250,00",
        &[
            "DITech".to_owned(),
            "Stadtwerke Musterstadt".to_owned(),
            "Telekom".to_owned(),
        ],
        &[
            "Rechnung".to_owned(),
            "Vertrag".to_owned(),
            "Mahnung".to_owned(),
        ],
        &[
            "Finanzen".to_owned(),
            "IT".to_owned(),
            "Geschäftlich".to_owned(),
        ],
        &["Invoice Number".to_owned(), "Total".to_owned()],
        &MetadataFieldFlags::ALL,
        &language,
        5,
        10,
        "This document is an invoice. Pay special attention to: invoice number (Rechnungsnummer / Rechnung Nr. / Invoice #), the GROSS total (Bruttobetrag / Gesamtbetrag / Total), and the issue date labeled as 'Rechnungsdatum' / 'Invoice date'. The correspondent is the issuer.",
    );
    println!(
        "════ SYSTEM PROMPT ({} chars) ════",
        request.system_prompt.len()
    );
    println!("{}", request.system_prompt);
    println!(
        "\n════ USER PROMPT ({} chars) ════",
        request.user_prompt.len()
    );
    println!("{}", request.user_prompt);
    println!("\n════ TEMPERATURE: {} ════", request.temperature);
    let schema = schema_for_metadata(
        &[
            "DITech".to_owned(),
            "Stadtwerke Musterstadt".to_owned(),
            "Telekom".to_owned(),
        ],
        &[
            "Rechnung".to_owned(),
            "Vertrag".to_owned(),
            "Mahnung".to_owned(),
        ],
        &[
            "Finanzen".to_owned(),
            "IT".to_owned(),
            "Geschäftlich".to_owned(),
        ],
        &["Invoice Number".to_owned(), "Total".to_owned()],
        &MetadataFieldFlags::ALL,
        5,
        10,
    )
    .expect("schema present when any key enabled");
    println!("\n════ RESPONSE SCHEMA (Ollama /api/chat format field) ════");
    println!("{}", serde_json::to_string_pretty(&schema).unwrap());
}
