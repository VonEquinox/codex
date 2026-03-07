# Sample configuration

For a sample configuration file, see [this documentation](https://developers.openai.com/codex/config-sample).

For reasoning-summary translation, add an experimental feature flag plus a dedicated translation provider/model in `config.toml`:

```toml
[features]
reasoning_summary_translation = true

[translation]
provider = "translator"
model = "gemini-2.5-flash-lite"

[model_providers.translator]
name = "Translator"
base_url = "https://example.com/v1"
api_key = "YOUR_TRANSLATOR_API_KEY"
wire_api = "responses"
```
