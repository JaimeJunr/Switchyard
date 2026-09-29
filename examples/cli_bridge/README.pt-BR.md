<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Bridge de CLIs do Switchyard

[English](README.md) | **Português (Brasil)**

> **Integração experimental:** o bridge e o seu comportamento podem mudar sem aviso.

O bridge de CLIs permite que o Switchyard use as ferramentas de linha de comando Claude Code,
Codex e Grok como modelos. Ele usa o login que você já tem em cada CLI. Você não precisa de uma
chave de API do provedor.

## Como funciona

1. O bridge é um pequeno servidor local. Ele fala a API OpenAI Chat Completions.
2. O Switchyard chama o bridge como qualquer outro upstream `openai_chat`.
3. Para cada requisição, o bridge roda um CLI em modo não interativo e devolve a resposta.

O ID do modelo escolhe o CLI:

| ID do modelo | CLI | Comando |
| --- | --- | --- |
| `claude` ou `claude/<modelo>` | Claude Code | `claude -p` |
| `codex` ou `codex/<modelo>` | Codex CLI | `codex exec` |
| `grok` ou `grok/<modelo>` | Grok CLI (xAI Grok Build) | `grok --prompt-file` |

A parte depois da barra vai para o CLI como `--model`, por exemplo `claude/opus` ou
`codex/gpt-5.5`. Sem ela, o CLI usa o modelo padrão dele.

O bridge é um workspace Cargo próprio. Ele não muda o `Cargo.toml` da raiz, o `Cargo.lock` da
raiz, nem nenhum crate do Switchyard.

## Requisitos

- Rust e Cargo. O arquivo `rust-toolchain.toml` do repositório escolhe a versão.
- Pelo menos um destes CLIs, instalado e com login feito:

| CLI | Login | Teste |
| --- | --- | --- |
| [Claude Code](https://code.claude.com/docs) | Rode `claude` uma vez e siga os passos de login. | `claude -p "Diga OK"` |
| [Codex CLI](https://developers.openai.com/codex/cli) | `codex login` | `codex exec --skip-git-repo-check "Diga OK"` |
| [Grok CLI](https://docs.x.ai/build/cli/headless-scripting) | `grok login`, ou defina `XAI_API_KEY` | `grok -p "Diga OK"` |

## Início rápido

Inicie o bridge. Ele escuta em `127.0.0.1:4100`:

```bash
cargo run --release --manifest-path examples/cli_bridge/Cargo.toml
```

Teste o bridge sozinho, em outro terminal:

```bash
curl http://127.0.0.1:4100/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model":"claude/haiku","messages":[{"role":"user","content":"Diga olá"}]}'
```

Inicie o Switchyard com as rotas de exemplo em [`routes.toml`](routes.toml):

```bash
switchyard-server --config examples/cli_bridge/routes.toml --dry-run
switchyard-server --config examples/cli_bridge/routes.toml --port 4000
```

Envie uma requisição pelo Switchyard:

```bash
curl http://127.0.0.1:4000/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model":"switchyard","messages":[{"role":"user","content":"Diga olá"}]}'
```

O arquivo de exemplo tem duas rotas:

| `id` da rota | Tipo | Alvos |
| --- | --- | --- |
| `switchyard` | Auto | Codex primeiro. Claude Opus quando os resultados das ferramentas mostram problemas. |
| `switchyard-task` | Task (`llm_classifier`) | Claude Haiku julga a tarefa. Grok para tarefas fáceis, Claude Opus para as difíceis. |

Edite os alvos para usar os CLIs e modelos que você tem. Você também pode misturar alvos do
bridge com alvos de API normais no mesmo arquivo.

## Usar um agente como cliente

O Switchyard aceita requisições OpenAI Chat Completions, OpenAI Responses e Anthropic Messages.
Aponte o seu agente para o Switchyard e use o `id` de uma rota como nome do modelo.

Inicie o bridge num terminal onde estas configurações não estejam ativas. Senão o CLI que o
bridge inicia também chamaria o Switchyard, em loop.

Claude Code:

```bash
ANTHROPIC_BASE_URL=http://127.0.0.1:4000 \
ANTHROPIC_MODEL=switchyard \
ANTHROPIC_DEFAULT_HAIKU_MODEL=switchyard \
claude
```

O Claude Code pode avisar que não conhece o modelo `switchyard`. O aviso não o impede de funcionar.

Codex CLI, com um perfil em `~/.codex/config.toml`:

```toml
[profiles.switchyard]
model = "switchyard"
model_provider = "switchyard"

[model_providers.switchyard]
name = "Switchyard"
base_url = "http://127.0.0.1:4000/v1"
wire_api = "responses"
```

```bash
codex --profile switchyard
```

## Limitações

- Os CLIs devolvem texto, não chamadas de ferramenta estruturadas. O bridge lista as
  ferramentas da requisição no prompt e pede ao modelo que responda com um pequeno objeto JSON
  quando quiser usar uma ferramenta. O bridge transforma esse objeto em chamadas de ferramenta
  normais. Se um modelo não seguir o formato, a resposta volta como texto simples.
- Imagens e arquivos nas mensagens são trocados por um aviso curto. O modelo não os vê.
- Uma requisição com streaming recebe a resposta inteira de uma vez, depois que o CLI termina.
- Cada requisição inicia um novo processo do CLI. Conte com alguns segundos a mais por chamada.
- `max_tokens`, `temperature` e outras configurações de amostragem são ignoradas.
- Claude Code e Grok informam o uso de tokens. O Codex não informa, então o bridge devolve zero.
- As flags dos CLIs mudam com o tempo. Se um CLI recusar uma flag, ajuste `arguments` em
  [`src/cli.rs`](src/cli.rs).
- Os termos de uso de cada CLI continuam valendo. Confira se o seu plano permite este uso.

## Segurança

- Por padrão, o bridge escuta em `127.0.0.1`. Não o exponha na rede. Quem conseguir acessá-lo
  pode usar os seus logins dos CLIs.
- Os CLIs rodam num diretório temporário novo e vazio, ou em `--workdir`.
- O Claude Code roda com as ferramentas internas e os servidores MCP desligados. O Codex roda
  num sandbox somente leitura. O Grok roda com as permissões padrão. O prompt também pede ao
  modelo que não use ferramentas próprias.

## Opções

| Flag | Padrão | Significado |
| --- | --- | --- |
| `--host` | `127.0.0.1` | Endereço de escuta. |
| `--port` | `4100` | Porta de escuta. |
| `--workdir` | novo diretório temporário | Diretório onde os CLIs rodam. |
| `--timeout-secs` | `600` | Tempo máximo de uma chamada de CLI. Depois disso, o bridge a interrompe e devolve HTTP 504. |
| `--claude-bin` | `claude` | Executável do Claude Code. |
| `--codex-bin` | `codex` | Executável do Codex CLI. |
| `--grok-bin` | `grok` | Executável do Grok CLI. |

Defina `RUST_LOG=debug` para ver mais detalhes no log. O log nunca inclui o texto do prompt.

## Comandos que o bridge roda

```text
claude -p --output-format json --tools "" --strict-mcp-config --no-session-persistence \
  --system-prompt <prompt curto fixo> [--model <modelo>]            # prompt pelo stdin
codex exec --skip-git-repo-check --ephemeral --sandbox read-only --color never \
  [--model <modelo>] -                                              # prompt pelo stdin
grok --prompt-file <arquivo temporário> --output-format json [--model <modelo>]
```

Um CLI que falha ou não consegue iniciar devolve HTTP 502 com o final da saída de erro. Um ID de
modelo desconhecido devolve HTTP 404.

## Endpoints

| Método e caminho | Função |
| --- | --- |
| `POST /v1/chat/completions` | Chat Completions, com ou sem streaming. |
| `GET /v1/models` | Lista `claude`, `codex` e `grok`. |
| `GET /health` | Devolve `{"status":"ok"}`. |

## Desenvolvimento

```bash
cd examples/cli_bridge
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

Os testes usam CLIs falsos escritos como scripts de shell. Eles não precisam de login nem de rede.
