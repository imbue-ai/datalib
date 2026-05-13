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
# Sync blocks: per-provider knobs that drive `mixed-up-files worker` when the
# source is `managed: true`. The same block is read by each downloader binary
# when run standalone (with --source-name <name>), so there's one source of
# truth for "what to pull from this provider". Discriminated on `kind` because
# a single provider can grow more than one sync mode over time.
# ---------------------------------------------------------------------------


class _SyncBase(BaseModel):
    refresh_window_days: int | None = None


class ClaudeWebSync(_SyncBase):
    kind: Literal["claude_web"]
    overlap: int | None = None


class ChatgptWebSync(_SyncBase):
    kind: Literal["chatgpt_web"]
    max_pages: int | None = None
    limit: int | None = None
    sleep_between: float | None = None


class SlackWebSync(_SyncBase):
    kind: Literal["slack_web"]
    channels: list[str] | None = None
    since: str | None = None
    all_channels: bool = False
    media: bool = True


class GithubWebSync(_SyncBase):
    kind: Literal["github_web"]
    max_prs: int | None = None


class GitlabWebSync(_SyncBase):
    kind: Literal["gitlab_web"]
    max_mrs: int | None = None


class NotionWebSync(_SyncBase):
    kind: Literal["notion_web"]
    subtree: str | None = None
    space: str | None = None
    notification_page_size: int | None = None
    max_notification_pages: int | None = None
    inbox_types: list[str] | None = None
    subtree_max_pages: int | None = None


SyncConfig = Annotated[
    ClaudeWebSync
    | ChatgptWebSync
    | SlackWebSync
    | GithubWebSync
    | GitlabWebSync
    | NotionWebSync,
    Field(discriminator="kind"),
]


class _SourceBase(BaseModel):
    name: str
    enabled: bool = True
    # `managed: true` = the worker is allowed to run this source's downloader
    # in response to UI-driven sync jobs. `managed: false` (default) = data
    # arrives via an external process (e.g. legacy export drops); the worker
    # only ingests what's already on disk.
    managed: bool = False
    sync: SyncConfig | None = None

    @field_validator("name")
    @classmethod
    def _name_nonempty(cls, v: str) -> str:
        if not v.strip():
            raise ValueError("source name must be non-empty")
        return v

    @model_validator(mode="after")
    def _managed_requires_sync(self) -> _SourceBase:
        if self.managed and self.sync is None:
            raise ValueError(
                f"source {self.name!r}: managed=true requires a `sync:` block"
            )
        return self


class AnthropicExportDirSource(_SourceBase):
    provider: Literal["anthropic"]
    kind: Literal["export_dir"]
    path: Path
    # 'export' for the bulk-download zip; 'api' for an incrementally-fetched
    # mirror (see scripts/sync_claude_web.py). API rows are authoritative
    # and survive an export re-ingest. See CLAUDE_WEB_SCHEMA.md.
    provenance: Literal["export", "api"] = "export"

    @field_validator("path", mode="after")
    @classmethod
    def _expand(cls, v: Path) -> Path:
        return Path(v).expanduser().resolve()


class ChatGPTApiDirSource(_SourceBase):
    provider: Literal["openai"]
    kind: Literal["chatgpt_api_dir"]
    path: Path
    # 'api' for the scraper output (scripts/sync_chatgpt_web.py). Only 'api'
    # is supported today — there's no bulk-export equivalent for ChatGPT.
    provenance: Literal["export", "api"] = "api"

    @field_validator("path", mode="after")
    @classmethod
    def _expand(cls, v: Path) -> Path:
        return Path(v).expanduser().resolve()


class SlackApiDirSource(_SourceBase):
    provider: Literal["slack"]
    kind: Literal["slack_api_dir"]
    path: Path

    @field_validator("path", mode="after")
    @classmethod
    def _expand(cls, v: Path) -> Path:
        return Path(v).expanduser().resolve()


class GithubApiDirSource(_SourceBase):
    provider: Literal["github"]
    kind: Literal["github_api_dir"]
    path: Path

    @field_validator("path", mode="after")
    @classmethod
    def _expand(cls, v: Path) -> Path:
        return Path(v).expanduser().resolve()


class GitlabApiDirSource(_SourceBase):
    provider: Literal["gitlab"]
    kind: Literal["gitlab_api_dir"]
    path: Path

    @field_validator("path", mode="after")
    @classmethod
    def _expand(cls, v: Path) -> Path:
        return Path(v).expanduser().resolve()


class NotionWebDirSource(_SourceBase):
    provider: Literal["notion"]
    kind: Literal["notion_web_dir"]
    path: Path

    @field_validator("path", mode="after")
    @classmethod
    def _expand(cls, v: Path) -> Path:
        return Path(v).expanduser().resolve()


SourceConfig = Annotated[
    AnthropicExportDirSource
    | ChatGPTApiDirSource
    | SlackApiDirSource
    | GithubApiDirSource
    | GitlabApiDirSource
    | NotionWebDirSource,
    Field(discriminator="provider"),
]


class Config(BaseModel):
    root: Path
    dolt: DoltConfig = Field(default_factory=DoltConfig)
    sources: list[SourceConfig] = Field(default_factory=list)

    @field_validator("root", mode="after")
    @classmethod
    def _expand_root(cls, v: Path) -> Path:
        return Path(v).expanduser().resolve()

    @model_validator(mode="after")
    def _unique_source_names(self) -> Config:
        names = [s.name for s in self.sources]
        dupes = {n for n in names if names.count(n) > 1}
        if dupes:
            raise ValueError(f"duplicate source names: {sorted(dupes)}")
        return self

    @property
    def enabled_sources(
        self,
    ) -> list[
        AnthropicExportDirSource
        | ChatGPTApiDirSource
        | SlackApiDirSource
        | GithubApiDirSource
        | GitlabApiDirSource
        | NotionWebDirSource
    ]:
        return [s for s in self.sources if s.enabled]


def load_config(path: Path | None = None) -> Config:
    cfg_path = Path(path).expanduser() if path else DEFAULT_CONFIG_PATH
    if not cfg_path.exists():
        raise FileNotFoundError(f"config not found: {cfg_path}")
    raw = yaml.safe_load(cfg_path.read_text()) or {}
    cfg = Config.model_validate(raw)
    cfg.root.mkdir(parents=True, exist_ok=True)
    return cfg
