from __future__ import annotations

from pathlib import Path
from typing import Annotated, Literal

import yaml
from pydantic import BaseModel, Field, field_validator, model_validator

DEFAULT_CONFIG_PATH = Path.home() / ".config" / "mixed-up-files" / "config.yaml"


class DoltConfig(BaseModel):
    port: int = 3306
    host: str = "127.0.0.1"
    user: str = "root"


# ---------------------------------------------------------------------------
# Sync blocks: per-source knobs that drive the worker's downloader child
# process and standalone `python -m ingest.run_source <name>` invocations.
# No discriminator on the sync block itself — the source's `type:` selects
# which sync schema applies (sync schema = "constructor arguments" for that
# source type). A source without a sync block is unmanaged: ingest-only.
# ---------------------------------------------------------------------------


class _SyncBase(BaseModel):
    refresh_window_days: int | None = None


class ClaudeApiSync(_SyncBase):
    overlap: int | None = None


class ChatgptApiSync(_SyncBase):
    max_pages: int | None = None
    limit: int | None = None
    sleep_between: float | None = None


class SlackApiSync(_SyncBase):
    channels: list[str] | None = None
    since: str | None = None
    all_channels: bool = False
    media: bool = True


class GithubApiSync(_SyncBase):
    max_prs: int | None = None


class GitlabApiSync(_SyncBase):
    max_mrs: int | None = None


class NotionInbox(BaseModel):
    enabled: bool = True
    types: list[str] | None = None
    notification_page_size: int | None = None
    max_notification_pages: int | None = None
    space: str | None = None


class NotionSubtrees(BaseModel):
    pages: list[str] = Field(default_factory=list)
    max_pages: int | None = None


class NotionApiSync(_SyncBase):
    # Both sub-blocks are optional, but at least one must drive a real
    # fetch — enforced by the model_validator below.
    inbox: NotionInbox | None = None
    subtrees: NotionSubtrees | None = None

    @model_validator(mode="after")
    def _at_least_one_mode(self) -> NotionApiSync:
        inbox_on = self.inbox is not None and self.inbox.enabled
        subtrees_on = self.subtrees is not None and bool(self.subtrees.pages)
        if not (inbox_on or subtrees_on):
            raise ValueError(
                "notion_api sync: must enable inbox or list at least one "
                "subtree page (set `inbox.enabled: true` and/or "
                "`subtrees.pages: [...]`)"
            )
        return self


# ---------------------------------------------------------------------------
# Sources: one `type:` discriminator. `type` collapses what used to be three
# fields (`provider`, `kind`, `provenance`) into one — think of `type:` as
# the name of a constructor and the rest of the source dict as its arguments.
# ---------------------------------------------------------------------------


class _SourceBase(BaseModel):
    name: str
    enabled: bool = True
    input_path: Path | None = None

    @field_validator("name")
    @classmethod
    def _name_nonempty(cls, v: str) -> str:
        if not v.strip():
            raise ValueError("source name must be non-empty")
        return v

    @field_validator("input_path", mode="after")
    @classmethod
    def _expand(cls, v: Path | None) -> Path | None:
        return Path(v).expanduser().resolve() if v is not None else None


class ClaudeExportSource(_SourceBase):
    """Anthropic bulk export — `claude.ai → "Export data"` zip, unpacked.
    No downloader (there is no API for the bulk zip), so this source must
    not carry a `sync:` block."""

    type: Literal["claude_export"]

    @property
    def provider(self) -> Literal["anthropic"]:
        return "anthropic"

    @property
    def provenance(self) -> Literal["export"]:
        return "export"

    @property
    def sync(self) -> None:
        # Convenience: lets call sites treat every source uniformly when
        # asking "is this source managed?" (i.e. `src.sync is not None`).
        return None


class _ManagedSourceBase(_SourceBase):
    """Sources that *can* be downloaded by the worker. The sync block is
    optional: presence ⇒ managed, absence ⇒ ingest-only."""


class ClaudeApiSource(_ManagedSourceBase):
    """Anthropic web-API scrape (`src/download/claude_web.py`). Provenance
    is `api`; merge code in `providers/anthropic/ingest.py` lets these rows
    win over `claude_export` rows for the same conversation."""

    type: Literal["claude_api"]
    sync: ClaudeApiSync | None = None

    @property
    def provider(self) -> Literal["anthropic"]:
        return "anthropic"

    @property
    def provenance(self) -> Literal["api"]:
        return "api"


class ChatgptApiSource(_ManagedSourceBase):
    type: Literal["chatgpt_api"]
    sync: ChatgptApiSync | None = None

    @property
    def provider(self) -> Literal["openai"]:
        return "openai"

    @property
    def provenance(self) -> Literal["api"]:
        return "api"


class SlackApiSource(_ManagedSourceBase):
    type: Literal["slack_api"]
    sync: SlackApiSync | None = None

    @property
    def provider(self) -> Literal["slack"]:
        return "slack"


class GithubApiSource(_ManagedSourceBase):
    type: Literal["github_api"]
    sync: GithubApiSync | None = None

    @property
    def provider(self) -> Literal["github"]:
        return "github"


class GitlabApiSource(_ManagedSourceBase):
    type: Literal["gitlab_api"]
    sync: GitlabApiSync | None = None

    @property
    def provider(self) -> Literal["gitlab"]:
        return "gitlab"


class NotionApiSource(_ManagedSourceBase):
    type: Literal["notion_api"]
    sync: NotionApiSync | None = None

    @property
    def provider(self) -> Literal["notion"]:
        return "notion"


SourceConfig = Annotated[
    ClaudeExportSource
    | ClaudeApiSource
    | ChatgptApiSource
    | SlackApiSource
    | GithubApiSource
    | GitlabApiSource
    | NotionApiSource,
    Field(discriminator="type"),
]


class Config(BaseModel):
    data_root: Path
    dolt: DoltConfig = Field(default_factory=DoltConfig)
    sources: list[SourceConfig] = Field(default_factory=list)

    @field_validator("data_root", mode="after")
    @classmethod
    def _expand_data_root(cls, v: Path) -> Path:
        return Path(v).expanduser().resolve()

    @model_validator(mode="after")
    def _unique_source_names(self) -> Config:
        names = [s.name for s in self.sources]
        dupes = {n for n in names if names.count(n) > 1}
        if dupes:
            raise ValueError(f"duplicate source names: {sorted(dupes)}")
        return self

    @model_validator(mode="after")
    def _fill_input_path_defaults(self) -> Config:
        for s in self.sources:
            if s.input_path is None:
                s.input_path = self.data_root / "raw" / s.name
        return self

    @property
    def enabled_sources(self) -> list[SourceConfig]:
        return [s for s in self.sources if s.enabled]


def load_config(path: Path | None = None) -> Config:
    cfg_path = Path(path).expanduser() if path else DEFAULT_CONFIG_PATH
    if not cfg_path.exists():
        raise FileNotFoundError(f"config not found: {cfg_path}")
    raw = yaml.safe_load(cfg_path.read_text()) or {}
    cfg = Config.model_validate(raw)
    cfg.data_root.mkdir(parents=True, exist_ok=True)
    return cfg
