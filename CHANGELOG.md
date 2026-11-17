# Changelog

## Unreleased (`release/v0.0.3-rc`)

- Engine pinned to intentumdiff-core `74618c3` (core #252, core PR #312: review completeness).
- An incomplete review (`completeness: "incomplete"`) is never published as clean: each of
  core's `incomplete.source_ranges` gets an Information diagnostic (`code:
  "incomplete_review"`) carrying core's `incomplete.message` verbatim (one at the top of the
  file when core recorded no range). Ranges are positioned by the core's source-aware mapper
  in the negotiated encoding; an incomplete review without its core object fails closed.
- New `quickfix` code action **Report IntentumDiff bug…** (`intentumdiff.reportIncompleteReview`):
  previews core's exact `report.title` and full `report.body_markdown` with
  `window/showMessageRequest` (Open / Cancel; a truncated report says the link carries a
  shortened body). Only Open calls `window/showDocument` with `external: true` on
  `report.issue_url`; Cancel or dismissal opens nothing. The server adds no paths or source
  text, never links the URL from the diagnostic, and never sends anything itself.
