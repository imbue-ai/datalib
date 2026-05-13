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


class _SourceBase(BaseModel):
    name: str
    enabled: bool = True

    @field_validator("name")
    @classmethod
    def _name_nonempty(cls, v: str) -> str:
        if not v.strip():
            raise ValueError("source name must be non-empty")
        return v


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
