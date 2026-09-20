# Template: create a Stage Epic

Use `mcp__youtrack__create_issue` (project `TL`) once per roadmap stage.

```json
{
  "project": "TL",
  "summary": "Stage <X.Y> — <one-line stage goal>",
  "description": "Roadmap/source: `index.md:<line>`, `ARCHITECTURE.md:<line>`.\n\n## Objective\n\n<Externally observable outcome and explicit non-goals.>\n\n## Baseline audit\n\n- Current source contracts inspected: <summary or link to a researcher comment>.\n- Existing tests and CI jobs mapped.\n- Documentation and package metadata checked.\n- Platform and privilege assumptions recorded.\n\n## Decisions required\n\n- <Decision; link the ADR Article ID under TL-A-1 once accepted.>",
  "customFields": {
    "Type": "Epic",
    "Stage": "Backlog",
    "Sprint": "<X.Y>"
  }
}
```

Then break the Epic into `User Story` children (one per bounded track/slice)
with `parentIssue` set to the new Epic's ID — see `story.md`.
