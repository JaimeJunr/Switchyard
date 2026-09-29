<p align="center">
  <img src="assets/logo.png" alt="Switchyard" width="800">
</p>

# NVIDIA NeMo Switchyard

[English](README.md) | **Português (Brasil)**

O Switchyard é uma biblioteca de código aberto que ajuda um agente de IA a escolher qual modelo atende cada requisição. Ele combina modelos eficientes com modelos mais capazes, para você equilibrar sucesso nas tarefas, custo e latência na sua carga de trabalho.

Use o Switchyard por meio de uma integração com um gateway, teste-o com um proxy local ou incorpore-o no seu próprio harness. Você escolhe o conjunto de modelos. O Switchyard fornece a decisão de roteamento. O seu gateway ou aplicação cuida do serviço em volta.

## Como funciona

1. Configure os modelos que a sua aplicação pode usar e escolha um algoritmo de roteamento.
2. O algoritmo examina a requisição ou a atividade recente de ferramentas do agente. Alguns algoritmos chamam um modelo para julgar a tarefa.
3. O seu gateway ou aplicação envia a requisição ao modelo escolhido. O roteamento pode mudar à medida que o agente continua o trabalho.

Avalie o agente completo, o conjunto de modelos e a configuração de roteamento contra a sua linha de base com um único modelo. Uma chamada de modelo mais barata não garante uma tarefa concluída com sucesso mais barata.

## Como usar

### Por um gateway existente

| Gateway | Comece aqui | Limites atuais |
| --- | --- | --- |
| **LiteLLM** | [Rode o exemplo do plugin de roteamento do Switchyard](examples/litellm/README.md#quick-start-with-the-local-proxy) | Experimental e só a partir do checkout. O exemplo fixa o LiteLLM 1.102.0 e suporta o roteamento Stage baseado no histórico da requisição, além do roteamento Random. Ele não atende chamadas de modelo intermediárias exigidas por algoritmos de classificação ou escalonamento. |
| **NeMo Relay** | [Compile e configure o plugin nativo](crates/switchyard-nemo-relay-plugin/README.md#build-from-source) | Requer Relay `>=0.8.0, <1.0.0`. A compilação a partir do código-fonte requer um toolchain Rust e Python 3. |

O Relay 0.8.x e 0.9.0 pode perder o status e os detalhes de erros do upstream quando o plugin
está ativo, mesmo para modelos fora das suas rotas. Leia a
[nota de compatibilidade de erros do upstream](docs/integrations/nemo_relay.md#upstream-error-compatibility)
antes de ativar o plugin.

Estes são caminhos de integração que você configura na sua própria implantação. Não são um endpoint hospedado do Switchyard. Siga as orientações de implantação de cada gateway para credenciais e operação do serviço.

### Teste o roteamento localmente

[Teste o roteamento localmente](docs/getting_started.md#server-path) com o proxy independente, para demonstrações e avaliação.

Há guias específicos para os agentes [pi](docs/integrations/pi.md) e
[Oh My Pi](docs/integrations/oh_my_pi.md).

### Use os CLIs Claude Code, Codex e Grok como modelos

O [bridge de CLIs](examples/cli_bridge/README.pt-BR.md) é um pequeno servidor local. Ele atende
requisições do Switchyard rodando `claude -p`, `codex exec` ou `grok` com o login que você já
tem em cada CLI. Assim você pode rotear entre esses serviços sem chave de API. O bridge fica em
`examples/cli_bridge/` e não muda nenhum crate do Switchyard.

### Incorpore a biblioteca no seu harness

[Incorpore a biblioteca no seu harness](docs/getting_started.md#library-path) para rodar o roteamento dentro da sua aplicação Rust. Para Python, veja o [exemplo de incorporação](examples/libsy.py).

## Algoritmos de roteamento

Comece com Auto. Escolha Task, Execution ou Composite quando precisar de mais controle. Estes nomes descrevem opções de roteamento. As chaves de configuração não mudam.

| Opção | Como escolhe | Configuração TOML |
| --- | --- | --- |
| **[Auto](docs/routing_algorithms/overview.md#auto)** | Usa o padrão atual: Execution (Stage), eficiente primeiro, com limiar de confiança de 0,5 e sem chamada de classificador. | `type = "auto"` |
| **[Task](docs/routing_algorithms/llm_classifier_routing.md)** | Um modelo julga se o modelo eficiente consegue fazer a tarefa. | `type = "llm_classifier"`, `mode = "capability"` |
| **[Execution](docs/routing_algorithms/stage_router_routing.md)** | Usa a atividade recente de ferramentas e sinais de resultado para escolher um modelo enquanto o agente trabalha. | `type = "stage_router"` |
| **[Composite](docs/routing_algorithms/composite_routing.md)** | Combina Task e Execution: um classificador define o nível de modelo padrão quando os sinais de execução são incertos. | `type = "composite"` |

Auto é um preset fixo na v0.3.0. Ele não compara estratégias em tempo de execução. O runner TOML suporta `type = "auto"`. Para incorporação direta em Python, use `stage_router(picker="efficient_first", confidence_threshold=0.5)` para o mesmo preset.

A [visão geral do roteamento](docs/routing_algorithms/overview.md) mantém o catálogo completo, incluindo as estratégias [Plan/Execute](docs/routing_algorithms/plan_execute_routing.md), [escalonamento](docs/routing_algorithms/escalation_router_routing.md), [advisor](docs/routing_algorithms/advisor_gate_routing.md), [sub-agente](docs/routing_algorithms/subagent_routing.md) e [random](docs/routing_algorithms/random_routing.md). O suporte varia por integração: confira o guia do gateway antes de escolher um algoritmo.

## Avaliação e referência

![Conclusão de tarefas versus custo para os roteamentos de classificação, stage e escalonamento do Switchyard, comparados com as linhas de base de modelo único Opus 4.8 e GLM 5.2.](assets/switchyard-cost-accuracy.png)

Os resultados dependem do benchmark, do conjunto de modelos, da pilha de serviço e da configuração de roteamento.
Para testes de latência e de sobrecarga do roteamento, veja [Soak Testing](docs/operations/soak_test.md).

### Leitura complementar

- [Configuração e perfis de benchmark](benchmark/README.md): rode as suas próprias comparações de linha de base e de roteamento.
- [Conceitos principais](docs/core_concepts.md): clientes, alvos, rotas e IDs de modelo.
- [Esquema TOML](docs/reference/toml_schema.md): campos de configuração e valores padrão.
- [Arquitetura](docs/architecture.md): componentes da biblioteca e do runtime.
- [Instalação](INSTALLATION.md): requisitos de pacote e de plataforma.

A documentação detalhada está em inglês.

## Componentes

Software pré-1.0. APIs, configuração e comportamento de roteamento podem mudar entre
versões. Fixe a versão que você integrar.

| Componente | Estabilidade | Use para | Orientação |
|---|---|---|---|
| `switchyard-libsy` | **Beta** | Roteamento incorporado no seu próprio gateway ou harness. Você cuida das chamadas de modelo, credenciais e novas tentativas. | Integrações de teste. A API vai mudar antes da v1.0. |
| `switchyard-llm-client` | **Alpha** | Chamadas HTTP de modelo e tradução de protocolo junto com a libsy. | Experimentos e pilotos. |
| `switchyard-runner` | **Alpha** | Rodar rotas configuradas dentro de outro runtime, como o NeMo Relay. | Trabalho de integração e pilotos supervisionados. |
| `switchyard-server` | **Demo** | Um proxy independente compatível com OpenAI e Anthropic. | Só para demonstrações e avaliação. Não é para produção. |

## Comunidade e licença

[Reporte um problema](https://github.com/NVIDIA-NeMo/Switchyard/issues) · [Contribua](CONTRIBUTING.md) · [Código de conduta](CODE_OF_CONDUCT.md)

[Apache 2.0](LICENSE). Copyright NVIDIA Corporation.
