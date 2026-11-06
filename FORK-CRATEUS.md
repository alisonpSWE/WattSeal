# WattSeal — fork Crateús/CE

Fork pessoal do [WattSeal](https://github.com/Daminoup88/WattSeal) para estimar quanto o PC (e o monitor) pesa na conta de luz.

## O que muda em relação ao original

| Recurso | Como funciona |
|---|---|
| **Tarifa automática** | Baixa a tarifa B1 residencial (TE + TUSD, "Tarifa de Aplicação") da **ENEL CE** e as **bandeiras tarifárias** da API de dados abertos da ANEEL a cada 6 h. Fica em cache no banco, então funciona offline; períodos conhecidos da ENEL CE vêm embutidos. Preço final = (TE + TUSD + bandeira) ÷ (1 − ICMS − PIS/COFINS). |
| **Gasto por mês** | Energia gravada por hora numa tabela própria (`energy_hourly`) que **nunca é apagada** pela limpeza do WattSeal. Cada dia usa a tarifa e a bandeira vigentes naquele dia. Ciclo pode começar em qualquer dia (dia de leitura da conta). |
| **Monitor** | Lê o brilho do monitor via DDC/CI a cada 5 s e sabe quando ele está ligado, em espera (Windows apagou a tela) ou desligado (sumiu da área de trabalho e sobrou só a tela virtual "VDD by MTT" do Sunshine/Moonlight). Modelo padrão AOC 27G4: 8 W (brilho 0) a 34 W (brilho 100), 0,3 W em espera. |
| **Pasta dedicada** | Tudo em `%LOCALAPPDATA%\WattSeal` (`power_monitoring.db`, `logs\collector.log`, `relatorios\`). Arquivos antigos ao lado do executável são migrados com segurança (checkpoint do WAL antes, nunca com outro WattSeal aberto). |
| **Relatório .md** | Aba **Conta de luz** → *Exportar .md* / *Copiar .md*, ou `WattSeal.exe --export-md`. Só meses, kWh, R$ e premissas (≈20 linhas), para colar numa IA. |
| **Backup legível** | `relatorios\historico-diario.csv` reescrito a cada 30 min (um dia por linha). |
| **Português (Brasil)** | Novo idioma na interface. |

## Ajustes (aba Conta de luz → Ajustes do cálculo)

- **ICMS** 20% (Ceará, Lei 18.305/2023) e **PIS+COFINS** ≈5,7% (varia todo mês, confira na sua conta).
- **Dia de leitura**: 1 = mês do calendário.
- **W extras**: consumo fixo que os sensores não veem (placa-mãe, fans, periféricos). Com um medidor de tomada dá para calibrar.
- **Eficiência da fonte**: fica na engrenagem de Configurações (padrão do WattSeal) e entra no cálculo.
- **Monitor**: W mínimo/máximo/espera, brilho assumido sem DDC/CI e filtro por id (ex.: `AOCB402`).

## Limitações

- Custo **marginal** do PC: não inclui CIP (iluminação pública) nem custo de disponibilidade.
- Consumo do PC é o que os sensores medem/estimam (CPU, GPU, RAM, disco, rede) mais os ajustes acima.
- Com o PC desligado/suspenso nada é contado (o monitor em standby gasta ~0,3 W).

## Desfazer

- Apagar `%LOCALAPPDATA%\WattSeal` remove todos os dados do fork e do WattSeal.
- As tabelas novas (`energy_hourly`, `tariff_periods`, `tariff_flags`, `billing_kv`, `monitor_status`) são ignoradas pelo WattSeal original, então o banco continua compatível.
