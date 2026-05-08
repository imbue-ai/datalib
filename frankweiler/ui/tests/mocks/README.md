# UI mocks — TNG-themed

Hand-curated JSON responses matching the routes that
`frankweiler/backend/http/src/lib.rs` serves. Used by Vitest tests and
useful as a drop-in mock for offline UI development.

| File                   | Mocked endpoint                       |
|------------------------|---------------------------------------|
| `search-response.json` | `GET /api/search?q=...`               |
| `chat-response.json`   | `GET /api/chat/{conversation_uuid}`   |

The content mirrors the ingestion fixtures under
`tests/fixtures/` (same UUIDs and names) so demos and
tests across layers tell a coherent story.

These mocks are **hand-edited** — see
`tests/fixtures/README.md` for the maintenance philosophy.
When the HTTP backend's response shape changes, update these files to match.

> Note: as of this commit, `/api/search` and `/api/chat` return
> placeholders from the Rust handler — the rows/messages fields are
> still empty server-side. These mocks describe the *intended* shape
> and can be wired in when F5/F6 land.
