//! Server-rendered Board. Refresh uses this same renderer, preventing browser/server drift.
use super::*;
use crate::owner_docs::{escape_html as e, page_shell_with_status};
use crate::watch_view::s;

fn array<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

pub(super) fn page(board: &Value) -> String {
    page_shell_with_status(
        "sm · Board",
        "board",
        "",
        &format!(
            r#"
<style>
.top {{ flex-wrap:wrap; gap:12px }}
.board-lane {{ margin:12px 0; padding:12px; border-left:3px solid var(--kl); background:var(--k1); border-radius:8px }}
.board-lane.unseen {{ border-left-color:var(--ka) }}
.board-row {{ padding:10px 0; border-top:1px solid var(--kl); overflow-wrap:anywhere }}
.board-detail {{ margin-top:5px; color:var(--kt2) }}
.board-warning {{ color:var(--ka) }}
#board button, #board input, #board select, #board-start input, #board-start select, #board-start textarea, #board-start button {{ font:inherit; color:var(--kt); background:var(--k2); border:1px solid var(--kl); border-radius:5px; padding:6px; max-width:100% }}
#board summary {{ cursor:pointer; overflow-wrap:anywhere }}
#board-start {{ background:var(--k1); color:var(--kt); border:1px solid var(--kl); border-radius:10px; width:min(95vw,650px) }}
#board-start::backdrop {{ background:#0009 }}
#board-start label {{ display:block; margin:10px 0 }}
#board-start textarea {{ display:block; width:100%; min-height:160px }}
#board-start input {{ width:100% }}
#board-message {{ color:var(--ka); overflow-wrap:anywhere }}
</style>
<h1>Board</h1><p id="board-message" role="status"></p>
<div id="board">{}</div>
<dialog id="board-start"><form id="board-start-form">
<h2 id="board-start-title">Start ticket</h2>
<label>Agent <select name="provider"><option value="claude">Claude</option><option value="codex-fork">Codex</option></select></label>
<label>Model <select name="model" required></select></label><p id="board-model-note" role="status"></p>
<label>Effort <select name="reasoning_effort"></select></label>
<label>Name <input name="name" maxlength="32" required></label>
<label>Brief <textarea name="brief" required></textarea></label>
<p id="board-start-error" role="alert"></p><button type="button" id="board-cancel">Cancel</button> <button id="board-submit" type="submit">Start</button>
</form></dialog>
<script>{}</script>"#,
            render(board),
            include_str!("board_client.js")
        ),
    )
}

fn link(url: &str, text: &str) -> String {
    // Links may originate in GitHub issue data or message contents.
    let url = if url.starts_with("https://")
        || url.starts_with("#lane-")
        || (url.starts_with('/') && !url.starts_with("//"))
    {
        url
    } else {
        "#"
    };
    format!(r#"<a class="lk" href="{}">{}</a>"#, e(url), e(text))
}

fn ticket_link(ticket: &Value) -> String {
    let url = format!(
        "https://github.com/{}/issues/{}",
        s(ticket, "repo"),
        ticket["number"]
    );
    link(&url, &format!("#{}", ticket["number"]))
}

fn row(ticket: &Value, head: Option<&Value>) -> String {
    let state = s(ticket, "state");
    let (label, color) = match state {
        "needs_you" => ("NEEDS YOU", "a"),
        "ready" => ("READY", "g"),
        "in_progress" => ("IN PROGRESS", "c"),
        "done" => ("DONE", ""),
        _ => ("BLOCKED", ""),
    };
    let mut details = Vec::new();
    let holder = &ticket["holder"];
    if holder.is_object() {
        details.push(link(
            "/watch",
            &format!("{} ({})", s(holder, "name"), s(holder, "state")),
        ));
    }
    let waits: Vec<String> = array(ticket, "waits_on")
        .iter()
        .filter(|t| s(t, "state") != "done")
        .map(ticket_link)
        .collect();
    if !waits.is_empty() {
        details.push(format!("waits on {}", waits.join(", ")));
    }
    for pr in array(ticket, "prs") {
        details.push(link(
            s(pr, "url"),
            &format!("PR #{} ({})", pr["number"], s(pr, "state")),
        ));
    }
    let needs = &ticket["needs_you"];
    if needs.is_object() {
        details.push(link(s(needs, "url"), s(needs, "text")));
        if head.is_some_and(|h| h["repo"] == ticket["repo"] && h["number"] == ticket["number"]) {
            details.push("heads the longest chain".into());
        }
    }
    if state == "done" {
        details.push(e(&s(ticket, "done_reason").replace('_', " ")));
    }
    for warning in array(ticket, "warnings") {
        let words = match warning.as_str().unwrap_or_default() {
            "working_while_blocked" => "working while blocked",
            "holder_stopped" | "agent_stopped" => "agent stopped",
            "merged_not_closed" => "PR merged — close the ticket",
            "cycle" | "in_cycle" => "waits in a loop",
            "stale" => "stale",
            other => other,
        };
        details.push(format!(
            r#"<span class="board-warning">{}</span>"#,
            e(words)
        ));
    }
    for lane in array(ticket, "also_in") {
        details.push(link(
            &format!("#lane-{}", lane["lane_id"]),
            &format!("also in lane {}", lane["rank"]),
        ));
    }
    let start = if state == "ready"
        && !array(ticket, "warnings")
            .iter()
            .any(|v| v == "merged_not_closed")
    {
        format!(
            r#"<button type="button" data-start="{}" data-repo="{}">Start</button>"#,
            ticket["number"],
            e(s(ticket, "repo"))
        )
    } else {
        String::new()
    };
    format!(
        r#"<div class="board-row"><div class="row"><span class="chip {color}">{label}</span>{} <strong>{}</strong> {} {start}</div><div class="board-detail">{}</div></div>"#,
        ticket_link(ticket),
        e(s(ticket, "title")),
        if ticket["new"] == true {
            "<span class=chip>new</span>"
        } else {
            ""
        },
        details.join(" · ")
    )
}

pub(super) fn render(board: &Value) -> String {
    let mut out = String::from(
        r#"<div class="bar"><form id="board-add"><label>Repo <input name="repo" list="board-repos" placeholder="owner/repo" required></label><label>Goal <input name="number" type="number" min="1" required></label><button>Add lane</button></form><button type="button" data-refresh>Refresh</button></div><datalist id="board-repos">"#,
    );
    for repo in array(board, "repos") {
        out.push_str(&format!(
            r#"<option value="{}"></option>"#,
            e(s(repo, "repo"))
        ));
    }
    out.push_str("</datalist>");
    for repo in array(board, "repos") {
        let age = s(repo, "last_ok_at");
        out.push_str(&format!(
            r#"<p class="m">{} · read from GitHub <time data-age="{}">{}</time></p>"#,
            e(s(repo, "repo")),
            e(age),
            if age.is_empty() {
                "never".into()
            } else {
                e(age)
            }
        ));
        if repo["stale"] == true {
            out.push_str(&format!(
                r#"<p class="board-warning">Stale: {} — {}</p>"#,
                e(s(repo, "repo")),
                e(s(repo, "error"))
            ));
        }
    }
    let lanes = array(board, "lanes");
    if lanes.is_empty() {
        out.push_str("<p>No active lanes. Add a goal ticket above.</p>");
    }
    for (index, lane) in lanes.iter().enumerate() {
        let id = lane["id"].as_i64().unwrap_or_default();
        out.push_str(&format!(r#"<details class="board-lane {}" id="lane-{id}" data-lane="{id}" {}><summary><strong>{} · {} · {} {}</strong></summary><div class="bar"><button type="button" data-move="-1" data-id="{id}" {} aria-label="Move lane up">↑</button><button type="button" data-move="1" data-id="{id}" {} aria-label="Move lane down">↓</button><button type="button" data-end="{id}">End</button><span data-confirm="{id}" hidden>End lane {}? <button type="button" data-end-yes="{id}">Yes</button> <button type="button" data-end-no="{id}">No</button></span></div>"#,
            if lane["unseen"] == true {"unseen"} else {""}, if lane["rank"] == 1 {"open"} else {""},lane["rank"],e(s(&lane["goal"],"repo")),ticket_link(&lane["goal"]),e(s(&lane["goal"],"title")), if index==0 {"disabled"}else{""},if index+1==lanes.len(){"disabled"}else{""}, lane["rank"]));
        let counts = &lane["counts"];
        out.push_str(&format!(
            "<p>{} needs you · {} ready · {} in progress · {} blocked · {} done</p>",
            counts["needs_you"],
            counts["ready"],
            counts["in_progress"],
            counts["blocked"],
            counts["done"]
        ));
        let chain = array(lane, "longest_chain");
        out.push_str(&format!(
            "<p>Longest chain {}: {}</p>",
            chain.len(),
            chain
                .iter()
                .map(ticket_link)
                .collect::<Vec<_>>()
                .join(" → ")
        ));
        if !array(lane, "cycles").is_empty() {
            out.push_str("<p class=board-warning>Tickets wait in a loop.</p>");
        }
        let tickets = array(lane, "tickets");
        for ticket in tickets.iter().filter(|t| s(t, "state") != "done") {
            out.push_str(&row(ticket, chain.first()));
        }
        out.push_str(&format!(
            "<details data-fold=\"done-{id}\"><summary>Done ({})</summary>",
            counts["done"]
        ));
        for ticket in tickets.iter().filter(|t| s(t, "state") == "done") {
            out.push_str(&row(ticket, None));
        }
        out.push_str(&format!(
            "</details><details data-fold=\"changes-{id}\"><summary>Recent changes</summary><ul>"
        ));
        for change in array(lane, "changes") {
            out.push_str(&format!("<li>{}</li>", e(s(change, "text"))));
        }
        out.push_str("</ul></details></details>");
    }
    out.push_str("<details data-fold=other><summary>Not in any lane</summary>");
    for group in array(board, "other") {
        out.push_str(&format!("<h2>{}</h2>", e(s(group, "repo"))));
        for ticket in array(group, "tickets") {
            out.push_str(&row(ticket, None));
        }
    }
    out.push_str("</details>");
    out
}
