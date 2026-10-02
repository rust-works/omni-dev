//! Object-addressed Slides reads; ungated like the other Drive read engines.
use super::api::SlidesApi;
use super::types::{CellLocation, Page, PageElement, Presentation};
use crate::cli::drive::format::{write_items_jsonl, JsonlSerialize};
use anyhow::Result;
use serde::Serialize;

/// Options for a full presentation fetch followed by client-side filtering.
#[derive(Debug, Clone)]
pub struct ReadOptions {
    /// Presentation to fetch.
    pub presentation_id: String,
    /// Ordinary slide IDs; empty means all slides.
    pub slides: Vec<String>,
}

/// One element (or table cell) with its object identities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SlideElement {
    /// One-based slide position.
    pub slide_index: usize,
    /// Owning ordinary slide.
    pub slide_object_id: String,
    /// Page containing the element (may be a notes page).
    pub page_object_id: String,
    /// Shape, table, group, image or unknown element ID.
    pub element_object_id: String,
    /// Object kind; notes elements use the separate notes kind.
    pub kind: String,
    /// Cell grid location when this row is a table cell.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cell: Option<CellLocation>,
    /// Concatenated runs of one shape/cell, never neighboring objects.
    pub text: String,
}

/// Full read result; JSONL emits one element per record.
#[derive(Debug, Clone, Serialize)]
pub struct ReadOutcome {
    /// Presentation identity.
    pub presentation_id: String,
    /// Presentation title.
    pub title: Option<String>,
    /// Editor-only revision token.
    pub revision_id: Option<String>,
    /// Ordered object rows, including separately identified notes.
    pub elements: Vec<SlideElement>,
}
impl JsonlSerialize for ReadOutcome {
    fn write_jsonl(&self, out: &mut dyn std::io::Write) -> Result<()> {
        write_items_jsonl(&self.elements, out)
    }
}

/// Fetches the full snapshot and then applies ordinary-slide filtering.
pub async fn read(api: &SlidesApi<'_>, opts: &ReadOptions) -> Result<ReadOutcome> {
    let presentation = api.get_presentation(&opts.presentation_id).await?;
    let pages = selected_slide_ids(&presentation, &opts.slides)?;
    Ok(ReadOutcome {
        presentation_id: opts.presentation_id.clone(),
        title: presentation.title.clone(),
        revision_id: presentation.revision_id.clone(),
        elements: flatten(&presentation, &pages, true),
    })
}

/// Validates and resolves a filter to unique ordinary-slide IDs in deck order.
/// These same IDs constrain both the preview and the wire mutation.
pub(in crate::drive) fn selected_slide_ids(
    p: &Presentation,
    requested: &[String],
) -> Result<Vec<String>> {
    for id in requested {
        anyhow::ensure!(
            !id.is_empty() && p.slides.iter().any(|page| &page.object_id == id),
            "Unknown slide id '{id}'; use `omni-dev drive slides info` to list ordinary slides"
        );
    }
    p.slides
        .iter()
        .filter(|page| requested.is_empty() || requested.contains(&page.object_id))
        .map(|page| {
            anyhow::ensure!(
                !page.object_id.is_empty(),
                "Slides returned an ordinary slide without an object id"
            );
            Ok(page.object_id.clone())
        })
        .collect()
}

/// Flattens ordinary slides and optionally their notes; templates are excluded.
pub fn flatten(p: &Presentation, pages: &[String], include_notes: bool) -> Vec<SlideElement> {
    let mut rows = Vec::new();
    for (index, slide) in p.slides.iter().enumerate() {
        if !pages.contains(&slide.object_id) {
            continue;
        }
        flatten_page(slide, index + 1, &slide.object_id, false, &mut rows);
        if include_notes {
            if let Some(notes) = slide
                .slide_properties
                .as_ref()
                .and_then(|s| s.notes_page.as_deref())
            {
                flatten_page(notes, index + 1, &slide.object_id, true, &mut rows);
            }
        }
    }
    rows
}

fn flatten_page(page: &Page, index: usize, slide: &str, notes: bool, rows: &mut Vec<SlideElement>) {
    for element in &page.page_elements {
        flatten_element(element, page, index, slide, notes, rows);
    }
}

fn flatten_element(
    e: &PageElement,
    page: &Page,
    index: usize,
    slide: &str,
    notes: bool,
    rows: &mut Vec<SlideElement>,
) {
    let row = |kind: &str, text: String, cell| SlideElement {
        slide_index: index,
        slide_object_id: slide.into(),
        page_object_id: page.object_id.clone(),
        element_object_id: e.object_id.clone(),
        kind: if notes { "notes" } else { kind }.into(),
        cell,
        text,
    };
    if let Some(shape) = &e.shape {
        rows.push(row(
            "shape",
            shape
                .text
                .as_ref()
                .map_or_else(String::new, super::types::Text::content),
            None,
        ));
    } else if let Some(table) = &e.table {
        for (r, table_row) in table.table_rows.iter().enumerate() {
            for (c, cell) in table_row.table_cells.iter().enumerate() {
                rows.push(row(
                    "table-cell",
                    cell.text
                        .as_ref()
                        .map_or_else(String::new, super::types::Text::content),
                    Some(cell.location.clone().unwrap_or(CellLocation {
                        row_index: r,
                        column_index: c,
                    })),
                ));
            }
        }
    } else if let Some(group) = &e.element_group {
        rows.push(row("group", String::new(), None));
        for child in &group.children {
            flatten_element(child, page, index, slide, notes, rows);
        }
    } else {
        rows.push(row(
            if e.image.is_some() {
                "image"
            } else {
                "unknown"
            },
            String::new(),
            None,
        ));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    fn fixture() -> Presentation {
        serde_json::from_value(serde_json::json!({"slides":[{"objectId":"s1","pageElements":[
            {"objectId":"g","elementGroup":{"children":[{"objectId":"g2","elementGroup":{"children":[{"objectId":"shape","shape":{"text":{"textElements":[{"textRun":{"content":"Q"}},{"textRun":{"content":"3"}}]}}}]}}]}},
            {"objectId":"table","table":{"tableRows":[{"tableCells":[{"location":{"rowIndex":0,"columnIndex":2},"text":{"textElements":[{"textRun":{"content":"cell"}}]}}]}]}},
            {"objectId":"image","image":{}},{"objectId":"future","newElement":{}}
        ],"slideProperties":{"notesPage":{"objectId":"n1","pageElements":[{"objectId":"note","shape":{"text":{"textElements":[{"textRun":{"content":"speaker"}}]}}}]}}}],"layouts":[{"objectId":"layout"}]})).unwrap()
    }
    #[test]
    fn object_graph_preserves_runs_groups_coordinates_and_notes() {
        let p = fixture();
        let rows = flatten(&p, &["s1".into()], true);
        assert_eq!(
            rows.iter().map(|r| r.kind.as_str()).collect::<Vec<_>>(),
            [
                "group",
                "group",
                "shape",
                "table-cell",
                "image",
                "unknown",
                "notes"
            ]
        );
        assert_eq!(rows[2].text, "Q3");
        assert_eq!(rows[3].cell.as_ref().unwrap().column_index, 2);
        assert_eq!(rows[6].page_object_id, "n1");
        assert_eq!(rows[6].slide_object_id, "s1");
        assert_eq!(flatten(&p, &["s1".into()], false).len(), 6);
    }
    #[test]
    fn filters_reject_templates_notes_and_unknown_ids_and_deduplicate() {
        let p = fixture();
        for id in ["layout", "n1", "wrong", ""] {
            assert!(selected_slide_ids(&p, &[id.into()]).is_err());
        }
        assert_eq!(
            selected_slide_ids(&p, &["s1".into(), "s1".into()]).unwrap(),
            ["s1"]
        );
    }
    #[test]
    fn jsonl_emits_element_rows_and_preserves_prose() {
        let p = fixture();
        let outcome = ReadOutcome {
            presentation_id: "p".into(),
            title: None,
            revision_id: None,
            elements: flatten(&p, &["s1".into()], true),
        };
        let mut buf = Vec::new();
        outcome.write_jsonl(&mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert_eq!(text.lines().count(), 7);
        let row: serde_json::Value = serde_json::from_str(text.lines().nth(2).unwrap()).unwrap();
        assert_eq!(row["text"], "Q3");
    }

    async fn serve_deck(server: &wiremock::MockServer) {
        use wiremock::matchers::{method, path};
        wiremock::Mock::given(method("GET"))
            .and(path("/v1/presentations/p1"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "title": "Deck",
                "revisionId": "rev-1",
                "slides": [
                    {"objectId":"s1","pageElements":[{"objectId":"a","shape":{"text":{"textElements":[{"textRun":{"content":"hi"}}]}}}]},
                    {"objectId":"s2","pageElements":[]}
                ]
            })))
            .expect(1)
            .mount(server)
            .await;
    }
    #[tokio::test]
    async fn read_returns_identity_and_rows_and_narrows_to_the_requested_slides() {
        use crate::drive::slides::client::test_support::mock_clients;
        for (filter, rows) in [
            (vec![], 1),
            (vec!["s1".to_string()], 1),
            (vec!["s2".to_string()], 0),
        ] {
            let server = wiremock::MockServer::start().await;
            let (_drive, slides) = mock_clients(&server).await;
            serve_deck(&server).await;
            let outcome = read(
                &SlidesApi::new(&slides),
                &ReadOptions {
                    presentation_id: "p1".into(),
                    slides: filter,
                },
            )
            .await
            .unwrap();
            assert_eq!(outcome.presentation_id, "p1");
            assert_eq!(outcome.title.as_deref(), Some("Deck"));
            assert_eq!(outcome.revision_id.as_deref(), Some("rev-1"));
            assert_eq!(outcome.elements.len(), rows);
        }
    }
    #[tokio::test]
    async fn read_rejects_an_unknown_slide_filter_after_the_single_fetch() {
        use crate::drive::slides::client::test_support::mock_clients;
        let server = wiremock::MockServer::start().await;
        let (_drive, slides) = mock_clients(&server).await;
        serve_deck(&server).await;
        let err = read(
            &SlidesApi::new(&slides),
            &ReadOptions {
                presentation_id: "p1".into(),
                slides: vec!["nope".into()],
            },
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("nope"), "{err}");
    }
}
