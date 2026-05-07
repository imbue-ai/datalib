from __future__ import annotations

from pathlib import Path
from typing import Annotated, Literal

import yaml
from pydantic import BaseModel, Field, field_validator, model_validator

DEFAULT_CONFIG_PATH = Path.home() / ".config" / "claude-mirror" / "config.yaml"


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

    @field_validator("path", mode="after")
    @classmethod
    def _expand(cls, v: Path) -> Path:
        return Path(v).expanduser().resolve()


SourceConfig = Annotated[
    AnthropicExportDirSource,
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
    def enabled_sources(self) -> list[AnthropicExportDirSource]:
        return [s for s in self.sources if s.enabled]


def load_config(path: Path | None = None) -> Config:
    cfg_path = Path(path).expanduser() if path else DEFAULT_CONFIG_PATH
    if not cfg_path.exists():
        raise FileNotFoundError(f"config not found: {cfg_path}")
    raw = yaml.safe_load(cfg_path.read_text()) or {}
    cfg = Config.model_validate(raw)
    cfg.root.mkdir(parents=True, exist_ok=True)
    return cfg
