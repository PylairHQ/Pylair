"""pylair-ollama — optional Ollama integration (M5).

Never a dependency of pylair core; apps opt in via their own deps and
@expose these helpers themselves. See integrations/ollama/README.md.
"""

from .client import (
    DEFAULT_HOST,
    Message,
    OllamaError,
    OllamaStatus,
    chat,
    normalize_host,
    pull,
    status,
)

__all__ = [
    "DEFAULT_HOST",
    "Message",
    "OllamaError",
    "OllamaStatus",
    "chat",
    "normalize_host",
    "pull",
    "status",
]
