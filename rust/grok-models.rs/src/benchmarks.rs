use phf::phf_map;

pub struct Scores {
    pub name: &'static str,
    pub intel: f32,
    pub coding: f32,
}

pub fn scores_for_live_id(mid: &str) -> Option<&'static Scores> {
    let slug = *MODEL_TO_SLUG.get(mid)?;
    if slug.is_empty() {
        return None;
    }
    BENCHMARKS.get(slug)
}

/// One catalog row. `slug` is the Artificial Analysis key.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BenchRow {
    pub name: &'static str,
    pub slug: &'static str,
    pub intel: f32,
    pub coding: f32,
}

/// Column order for Shift+S: name, slug, intel, coding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BenchSort {
    Name,
    Slug,
    Intel,
    Coding,
}

impl BenchSort {
    pub fn cycle(self) -> Self {
        match self {
            Self::Name => Self::Slug,
            Self::Slug => Self::Intel,
            Self::Intel => Self::Coding,
            Self::Coding => Self::Name,
        }
    }

    pub fn column(self) -> usize {
        match self {
            Self::Name => 0,
            Self::Slug => 1,
            Self::Intel => 2,
            Self::Coding => 3,
        }
    }
}

/// Every `BENCHMARKS` entry, filtered by name or slug, then sorted.
/// Intel and coding sort highest first. An empty query returns the full map.
pub fn rows(query: &str, sort: BenchSort) -> Vec<BenchRow> {
    let term = query.to_lowercase();
    let mut out: Vec<BenchRow> = BENCHMARKS
        .entries()
        .filter(|(slug, scores)| {
            term.is_empty()
                || scores.name.to_lowercase().contains(&term)
                || slug.to_lowercase().contains(&term)
        })
        .map(|(slug, scores)| BenchRow {
            name: scores.name,
            slug: *slug,
            intel: scores.intel,
            coding: scores.coding,
        })
        .collect();
    out.sort_by(|a, b| match sort {
        BenchSort::Name => a
            .name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.slug.cmp(b.slug)),
        BenchSort::Slug => a.slug.cmp(b.slug),
        BenchSort::Intel => cmp_score_desc(a.intel, b.intel).then_with(|| a.slug.cmp(b.slug)),
        BenchSort::Coding => cmp_score_desc(a.coding, b.coding).then_with(|| a.slug.cmp(b.slug)),
    });
    out
}

fn cmp_score_desc(a: f32, b: f32) -> std::cmp::Ordering {
    b.partial_cmp(&a).unwrap_or(std::cmp::Ordering::Equal)
}

// Keys in both maps stay sorted ascending.
pub static BENCHMARKS: phf::Map<&'static str, Scores> = phf_map! {
    "claude-4-5-haiku-reasoning" => Scores { name: "Claude 4.5 Haiku (Reasoning)", intel: 17.6, coding: 43.9 },
    "claude-4-5-sonnet-thinking" => Scores { name: "Claude 4.5 Sonnet (Reasoning)", intel: 21.2, coding: 52.1 },
    "claude-4-sonnet-thinking" => Scores { name: "Claude 4 Sonnet (Reasoning)", intel: 18.9, coding: 37.6 },
    "claude-fable-5" => Scores { name: "Claude Fable 5 (Adaptive Reasoning, Max Effort, Opus 4.8 Fallback)", intel: 49.7, coding: 76.5 },
    "claude-fable-5-1" => Scores { name: "Claude Fable 5.1 (Adaptive Reasoning, Max Effort, Default Fallback)", intel: 53.4, coding: 81.6 },
    "claude-opus-4-5" => Scores { name: "Claude Opus 4.5 (Non-reasoning)", intel: 23.7, coding: 0.0 },
    "claude-opus-4-6" => Scores { name: "Claude Opus 4.6 (Non-reasoning, High Effort)", intel: 26.4, coding: 0.0 },
    "claude-opus-4-7" => Scores { name: "Claude Opus 4.7 (Adaptive Reasoning, Max Effort)", intel: 40.7, coding: 73.6 },
    "claude-opus-4-8" => Scores { name: "Claude Opus 4.8 (Adaptive Reasoning, Max Effort)", intel: 42.0, coding: 74.3 },
    "claude-opus-5" => Scores { name: "Claude Opus 5 (Adaptive Reasoning, Max Effort)", intel: 50.7, coding: 78.0 },
    "claude-opus-5-5" => Scores { name: "Claude Opus 5.5 (Adaptive Reasoning, Max Effort, Default Fallback)", intel: 57.6, coding: 0.0 },
    "claude-sonnet-4-6" => Scores { name: "Claude Sonnet 4.6 (Non-reasoning, High Effort)", intel: 24.7, coding: 0.0 },
    "claude-sonnet-5" => Scores { name: "Claude Sonnet 5 (Adaptive Reasoning, Max Effort)", intel: 38.4, coding: 71.5 },
    "deepseek-v4-1-flash" => Scores { name: "DeepSeek V4.1 Flash (Reasoning, Max Effort)", intel: 39.5, coding: 0.0 },
    "deepseek-v4-flash" => Scores { name: "DeepSeek V4 Flash 0731 (Reasoning, Max Effort)", intel: 34.5, coding: 69.1 },
    "deepseek-v4-flash-vision" => Scores { name: "DeepSeek V4 Flash Vision (Reasoning, Max Effort)", intel: 35.0, coding: 65.0 },
    "deepseek-v4-pro" => Scores { name: "DeepSeek V4 Pro 0813 (Reasoning, Max Effort)", intel: 36.3, coding: 68.8 },
    "gemini-3-6-flash" => Scores { name: "Gemini 3.6 Flash (high)", intel: 34.3, coding: 69.2 },
    "gemini-3-7-flash" => Scores { name: "Gemini 3.7 Flash (high)", intel: 39.4, coding: 76.1 },
    "gemini-3-8-flash" => Scores { name: "Gemini 3.8 Flash (high)", intel: 41.2, coding: 76.3 },
    "gemma-4-26b-a4b" => Scores { name: "Gemma 4 26B A4B (Reasoning)", intel: 16.7, coding: 39.3 },
    "gemma-4-31b" => Scores { name: "Gemma 4 31B (Reasoning)", intel: 15.4, coding: 43.4 },
    "glm-5" => Scores { name: "GLM-5 (Reasoning)", intel: 27.9, coding: 0.0 },
    "glm-5-1" => Scores { name: "GLM-5.1 (Reasoning)", intel: 26.4, coding: 55.8 },
    "glm-5-2" => Scores { name: "GLM-5.2 (max)", intel: 34.0, coding: 68.8 },
    "glm-5-3" => Scores { name: "GLM-5.3 (max)", intel: 44.9, coding: 74.8 },
    "glm-5-3-flash" => Scores { name: "GLM-5.3-Flash", intel: 41.9, coding: 71.5 },
    "gpt-4" => Scores { name: "GPT-4", intel: 6.7, coding: 13.1 },
    "gpt-4-1" => Scores { name: "GPT-4.1", intel: 12.7, coding: 0.0 },
    "gpt-4o" => Scores { name: "GPT-4o (Nov '24)", intel: 8.4, coding: 0.0 },
    "gpt-4o-mini" => Scores { name: "GPT-4o mini", intel: 6.7, coding: 11.4 },
    "gpt-5" => Scores { name: "GPT-5 (high)", intel: 23.0, coding: 37.8 },
    "gpt-5-1" => Scores { name: "GPT-5.1 (high)", intel: 24.7, coding: 49.4 },
    "gpt-5-2" => Scores { name: "GPT-5.2 (xhigh)", intel: 30.4, coding: 0.0 },
    "gpt-5-4" => Scores { name: "GPT-5.4 (xhigh)", intel: 39.0, coding: 71.1 },
    "gpt-5-5" => Scores { name: "GPT-5.5 (xhigh)", intel: 38.6, coding: 74.9 },
    "gpt-5-6-luna" => Scores { name: "GPT-5.6 Luna (max)", intel: 37.5, coding: 71.4 },
    "gpt-5-6-sol" => Scores { name: "GPT-5.6 Sol (max)", intel: 47.1, coding: 77.4 },
    "gpt-5-6-terra" => Scores { name: "GPT-5.6 Terra (max)", intel: 42.3, coding: 76.7 },
    "gpt-6-astra" => Scores { name: "GPT-6 Astra (max)", intel: 52.8, coding: 76.9 },
    "gpt-6-luna" => Scores { name: "GPT-6 Luna (max)", intel: 37.3, coding: 0.0 },
    "gpt-6-sol" => Scores { name: "GPT-6 Sol (max)", intel: 47.5, coding: 0.0 },
    "gpt-oss-120b" => Scores { name: "gpt-oss-120b (high)", intel: 12.3, coding: 30.4 },
    "gpt-oss-20b" => Scores { name: "gpt-oss-20b (high)", intel: 9.0, coding: 20.7 },
    "grok-4-5" => Scores { name: "Grok 4.5 (high)", intel: 39.1, coding: 72.4 },
    "grok-4-6" => Scores { name: "Grok 4.6 (high)", intel: 44.4, coding: 76.8 },
    "grok-4-7-high" => Scores { name: "Grok 4.7 (high)", intel: 46.3, coding: 0.0 },
    "hy3" => Scores { name: "Hy3", intel: 25.8, coding: 58.8 },
    "inkling" => Scores { name: "Inkling (xhigh)", intel: 25.5, coding: 52.1 },
    "inkling-small" => Scores { name: "Inkling Small", intel: 26.1, coding: 52.9 },
    "kimi-k2-7-code" => Scores { name: "Kimi K2.7 Code", intel: 26.3, coding: 60.8 },
    "kimi-k3" => Scores { name: "Kimi K3 (max)", intel: 43.8, coding: 76.2 },
    "ling-3-0-flash" => Scores { name: "Ling 3.0 Flash", intel: 20.6, coding: 50.6 },
    "ling-3-0-flash-vl" => Scores { name: "Ling-3.0-flash-VL", intel: 25.0, coding: 57.0 },
    "longcat-2-0" => Scores { name: "LongCat 2.0", intel: 19.7, coding: 45.3 },
    "mimo-v2-5-0424" => Scores { name: "MiMo-V2.5", intel: 22.3, coding: 56.8 },
    "mimo-v2-5-pro" => Scores { name: "MiMo-V2.5-Pro", intel: 26.4, coding: 60.2 },
    "mimo-v2-6-pro" => Scores { name: "MiMo-V2.6-Pro", intel: 46.3, coding: 0.0 },
    "minimax-m3" => Scores { name: "MiniMax-M3", intel: 29.6, coding: 58.6 },
    "mistral-large-3" => Scores { name: "Mistral Large 3", intel: 9.7, coding: 20.1 },
    "mistral-small-4" => Scores { name: "Mistral Small 4 (Reasoning)", intel: 11.5, coding: 26.6 },
    "muse-glimmer" => Scores { name: "Muse Glimmer (high)", intel: 18.1, coding: 49.0 },
    "muse-spark-1-2" => Scores { name: "Muse Spark 1.2 (xhigh)", intel: 39.8, coding: 72.2 },
    "muse-spark-1-3" => Scores { name: "Muse Spark 1.3 (max)", intel: 48.2, coding: 75.8 },
    "nemotron-3-5-lightning" => Scores { name: "Nemotron 3.5 Lightning", intel: 13.6, coding: 26.8 },
    "nemotron-3-nano-omni-30b-a3b" => Scores { name: "Nemotron 3 Nano Omni 30B A3B Reasoning", intel: 10.3, coding: 13.8 },
    "north-mini-code" => Scores { name: "North Mini Code", intel: 9.9, coding: 36.5 },
    "nova-2-0-lite-reasoning" => Scores { name: "Nova 2.0 Lite (high)", intel: 13.4, coding: 23.0 },
    "nova-lite" => Scores { name: "Nova Lite", intel: 6.7, coding: 0.0 },
    "nova-micro" => Scores { name: "Nova Micro", intel: 5.9, coding: 0.0 },
    "nova-premier" => Scores { name: "Nova Premier", intel: 9.2, coding: 0.0 },
    "nova-pro" => Scores { name: "Nova Pro", intel: 7.0, coding: 0.0 },
    "nvidia-nemotron-3-nano-30b-a3b-reasoning" => Scores { name: "NVIDIA Nemotron 3 Nano 30B A3B (Reasoning)", intel: 8.9, coding: 14.4 },
    "nvidia-nemotron-3-super-120b-a12b" => Scores { name: "Nemotron 3 Super 120B A12B (Reasoning)", intel: 13.6, coding: 37.7 },
    "nvidia-nemotron-3-ultra-550b-a55b" => Scores { name: "Nemotron 3 Ultra 550B A55B (Reasoning)", intel: 23.4, coding: 49.3 },
    "qwen3-5-397b-a17b" => Scores { name: "Qwen3.5 397B A17B (Reasoning)", intel: 19.1, coding: 48.2 },
    "qwen3-7-max" => Scores { name: "Qwen3.7 Max", intel: 29.9, coding: 66.0 },
    "qwen3-7-plus" => Scores { name: "Qwen3.7 Plus", intel: 25.8, coding: 55.9 },
    "qwen3-8-27b" => Scores { name: "Qwen3.8 27B (xhigh)", intel: 33.7, coding: 68.1 },
    "qwen3-8-flash-next" => Scores { name: "Qwen3.8-Flash-Next", intel: 39.9, coding: 73.1 },
    "qwen3-8-max" => Scores { name: "Qwen3.8 Max (0902)", intel: 45.4, coding: 76.2 },
    "step-3-7-flash" => Scores { name: "Step 3.7 Flash", intel: 19.5, coding: 39.6 },
};

pub static MODEL_TO_SLUG: phf::Map<&'static str, &'static str> = phf_map! {
    "amazon/nova-2-lite-v1" => "nova-2-0-lite-reasoning",
    "amazon/nova-lite-v1" => "nova-lite",
    "amazon/nova-micro-v1" => "nova-micro",
    "amazon/nova-premier-v1" => "nova-premier",
    "amazon/nova-pro-v1" => "nova-pro",
    "anthropic/claude-fable-5" => "claude-fable-5",
    "anthropic/claude-fable-5.1" => "claude-fable-5-1",
    "anthropic/claude-opus-4.6" => "claude-opus-4-6",
    "anthropic/claude-opus-4.7" => "claude-opus-4-7",
    "anthropic/claude-opus-4.8" => "claude-opus-4-8",
    "anthropic/claude-opus-5" => "claude-opus-5",
    "anthropic/claude-opus-5.5" => "claude-opus-5-5",
    "anthropic/claude-sonnet-5" => "claude-sonnet-5",
    "claude-fable-5" => "claude-fable-5",
    "claude-fable-5-1" => "claude-fable-5-1",
    "claude-haiku-4-5" => "claude-4-5-haiku-reasoning",
    "claude-opus-4-5" => "claude-opus-4-5",
    "claude-opus-4-6" => "claude-opus-4-6",
    "claude-opus-4-7" => "claude-opus-4-7",
    "claude-opus-4-8" => "claude-opus-4-8",
    "claude-opus-5" => "claude-opus-5",
    "claude-opus-5-5" => "claude-opus-5-5",
    "claude-sonnet-4" => "claude-4-sonnet-thinking",
    "claude-sonnet-4-5" => "claude-4-5-sonnet-thinking",
    "claude-sonnet-4-6" => "claude-sonnet-4-6",
    "claude-sonnet-5" => "claude-sonnet-5",
    "cohere/north-mini-code:free" => "north-mini-code",
    "deepseek-ai/DeepSeek-V4.1-Flash" => "deepseek-v4-1-flash",
    "deepseek-v4-flash" => "deepseek-v4-flash",
    "deepseek-v4-flash-free" => "deepseek-v4-flash",
    "deepseek-v4-flash-vision-exp" => "deepseek-v4-flash-vision",
    "deepseek-v4-flash:0731-cloud" => "deepseek-v4-flash",
    "deepseek-v4-pro" => "deepseek-v4-pro",
    "deepseek-v4.1-flash" => "deepseek-v4-1-flash",
    "deepseek-v4.1-flash:cloud" => "deepseek-v4-1-flash",
    "deepseek/deepseek-v4-flash-0731" => "deepseek-v4-flash",
    "deepseek/deepseek-v4-flash-vision-exp" => "deepseek-v4-flash-vision",
    "deepseek/deepseek-v4.1-flash" => "deepseek-v4-1-flash",
    "dots-studio/dots-3-note-preview:free" => "",
    "gemini-3.6-flash" => "gemini-3-6-flash",
    "gemini-3.7-flash" => "gemini-3-7-flash",
    "gemini-3.8-flash" => "gemini-3-8-flash",
    "gemma4:31b-cloud" => "gemma-4-31b",
    "glm-5.2" => "glm-5-2",
    "glm-5.2:cloud" => "glm-5-2",
    "glm-5.3" => "glm-5-3",
    "glm-5.3-flash" => "glm-5-3-flash",
    "glm-5.3-flash:cloud" => "glm-5-3-flash",
    "glm-5.3:cloud" => "glm-5-3",
    "google/gemini-3.6-flash" => "gemini-3-6-flash",
    "google/gemini-3.7-flash" => "gemini-3-7-flash",
    "google/gemini-3.8-flash" => "gemini-3-8-flash",
    "google/gemma-4-26b-a4b-it:free" => "gemma-4-26b-a4b",
    "google/gemma-4-31b-it" => "gemma-4-31b",
    "google/gemma-4-31b-it:free" => "gemma-4-31b",
    "gpt-5.4" => "gpt-5-4",
    "gpt-5.5" => "gpt-5-5",
    "gpt-5.6-luna" => "gpt-5-6-luna",
    "gpt-5.6-sol" => "gpt-5-6-sol",
    "gpt-5.6-terra" => "gpt-5-6-terra",
    "gpt-6-astra" => "gpt-6-astra",
    "gpt-6-luna" => "gpt-6-luna",
    "gpt-6-sol" => "gpt-6-sol",
    "gpt-oss:120b-cloud" => "gpt-oss-120b",
    "grok-4.5" => "grok-4-5",
    "grok-4.6" => "grok-4-6",
    "grok-4.7" => "grok-4-7-high",
    "hy3" => "hy3",
    "hy4-preview" => "",
    "inclusionai/ling-3.0-flash" => "ling-3-0-flash",
    "inclusionai/ling-3.0-flash-fin" => "ling-3-0-flash",
    "inclusionai/ling-3.0-flash-fin:free" => "ling-3-0-flash",
    "inclusionai/ling-3.0-flash-sante:free" => "ling-3-0-flash",
    "inclusionai/ling-3.0-flash-vl" => "ling-3-0-flash-vl",
    "inclusionai/ling-3.0-flash-vl:free" => "ling-3-0-flash-vl",
    "kimi-k2.7-code" => "kimi-k2-7-code",
    "kimi-k2.7-code:cloud" => "kimi-k2-7-code",
    "kimi-k3" => "kimi-k3",
    "kimi-k3:cloud" => "kimi-k3",
    "ling-3.0-flash-fin-free" => "ling-3-0-flash",
    "liquid/lfm-2.5-2.6b:free" => "",
    "longcat-2.0" => "longcat-2-0",
    "meta/muse-glimmer-30b" => "muse-glimmer",
    "meta/muse-spark-1.2" => "muse-spark-1-2",
    "meta/muse-spark-1.2-contributor" => "muse-spark-1-2",
    "meta/muse-spark-1.3" => "muse-spark-1-3",
    "meta/muse-spark-1.3-contributor" => "muse-spark-1-3",
    "mimo-v2.5" => "mimo-v2-5-0424",
    "mimo-v2.5-free" => "mimo-v2-5-0424",
    "mimo-v2.5-pro" => "mimo-v2-5-pro",
    "mimo-v2.6-pro" => "mimo-v2-6-pro",
    "minimax-m3" => "minimax-m3",
    "minimax-m3:cloud" => "minimax-m3",
    "minimax/minimax-m3" => "minimax-m3",
    "mistral-large-3:675b-cloud" => "mistral-large-3",
    "mistralai/mistral-nemo" => "",
    "mistralai/mistral-small-2603" => "mistral-small-4",
    "moonshotai/kimi-k2.7-code" => "kimi-k2-7-code",
    "moonshotai/kimi-k3" => "kimi-k3",
    "muse-spark-1.2" => "muse-spark-1-2",
    "muse-spark-1.2-contributor" => "muse-spark-1-2",
    "muse-spark-1.2-contributor-free" => "muse-spark-1-2",
    "muse-spark-1.3" => "muse-spark-1-3",
    "muse-spark-1.3-contributor" => "muse-spark-1-3",
    "muse-spark-1.3-contributor-free" => "muse-spark-1-3",
    "nemotron-3-nano:30b-cloud" => "nvidia-nemotron-3-nano-30b-a3b-reasoning",
    "nemotron-3-super:cloud" => "nvidia-nemotron-3-super-120b-a12b",
    "nemotron-3-ultra-free" => "nvidia-nemotron-3-ultra-550b-a55b",
    "nemotron-3-ultra:cloud" => "nvidia-nemotron-3-ultra-550b-a55b",
    "nemotron-3.5-lightning-free" => "nemotron-3-5-lightning",
    "nex-agi/nex-n2.5-mini:free" => "",
    "nex-agi/nex-n2.5-pro:free" => "",
    "nvidia/NVIDIA-Nemotron-3.5-Lightning-30B-A3B-BF16" => "nemotron-3-5-lightning",
    "nvidia/nemotron-3-nano-omni-30b-a3b-reasoning:free" => "nemotron-3-nano-omni-30b-a3b",
    "nvidia/nemotron-3-super-120b-a12b:free" => "nvidia-nemotron-3-super-120b-a12b",
    "nvidia/nemotron-3-ultra-550b-a55b" => "nvidia-nemotron-3-ultra-550b-a55b",
    "nvidia/nemotron-3-ultra-550b-a55b:free" => "nvidia-nemotron-3-ultra-550b-a55b",
    "nvidia/nemotron-3.5-content-safety:free" => "",
    "nvidia/nemotron-3.5-lightning" => "nemotron-3-5-lightning",
    "nvidia/nemotron-3.5-lightning:free" => "nemotron-3-5-lightning",
    "openai/gpt-4" => "gpt-4",
    "openai/gpt-4.1" => "gpt-4-1",
    "openai/gpt-4o" => "gpt-4o",
    "openai/gpt-4o-mini" => "gpt-4o-mini",
    "openai/gpt-5" => "gpt-5",
    "openai/gpt-5.1" => "gpt-5-1",
    "openai/gpt-5.2" => "gpt-5-2",
    "openai/gpt-5.4" => "gpt-5-4",
    "openai/gpt-5.5" => "gpt-5-5",
    "openai/gpt-5.6-luna" => "gpt-5-6-luna",
    "openai/gpt-5.6-luna-pro" => "gpt-5-6-luna",
    "openai/gpt-5.6-sol" => "gpt-5-6-sol",
    "openai/gpt-5.6-sol-pro" => "gpt-5-6-sol",
    "openai/gpt-5.6-terra" => "gpt-5-6-terra",
    "openai/gpt-5.6-terra-pro" => "gpt-5-6-terra",
    "openai/gpt-6-astra" => "gpt-6-astra",
    "openai/gpt-6-astra-pro" => "gpt-6-astra",
    "openai/gpt-oss-120b" => "gpt-oss-120b",
    "openai/gpt-oss-20b" => "gpt-oss-20b",
    "openrouter/free" => "",
    "poolside/laguna-s-2.1" => "",
    "poolside/laguna-s-2.1:free" => "",
    "poolside/laguna-xs-2.1" => "",
    "poolside/laguna-xs-2.1:free" => "",
    "qwen/qwen3.8-27b" => "qwen3-8-27b",
    "qwen/qwen3.8-27b:free" => "qwen3-8-27b",
    "qwen/qwen3.8-flash" => "qwen3-8-flash-next",
    "qwen/qwen3.8-max-0902" => "qwen3-8-max",
    "qwen3.5:397b-cloud" => "qwen3-5-397b-a17b",
    "qwen3.7-max" => "qwen3-7-max",
    "qwen3.7-plus" => "qwen3-7-plus",
    "qwen3.8-flash" => "qwen3-8-flash-next",
    "qwen3.8-max" => "qwen3-8-max",
    "stepfun/step-3.7-flash:free" => "step-3-7-flash",
    "tencent/Hy3" => "hy3",
    "tencent/hy3" => "hy3",
    "tencent/hy4-preview" => "",
    "thinkingmachines/inkling-small:free" => "inkling-small",
    "thinkingmachines/inkling:free" => "inkling",
    "x-ai/grok-4.5" => "grok-4-5",
    "x-ai/grok-4.6" => "grok-4-6",
    "x-ai/grok-4.7" => "grok-4-7-high",
    "xiaomi/mimo-v2.5" => "mimo-v2-5-0424",
    "z-ai/glm-5" => "glm-5",
    "z-ai/glm-5.1" => "glm-5-1",
    "z-ai/glm-5.2" => "glm-5-2",
    "z-ai/glm-5.2:free" => "glm-5-2",
    "z-ai/glm-5.3" => "glm-5-3",
    "z-ai/glm-5.3-flash" => "glm-5-3-flash",
    "zai-org/GLM-5.3" => "glm-5-3",
    "zai-org/GLM-5.3-Flash" => "glm-5-3-flash",
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_filter_and_sort_from_the_map() {
        let all = rows("", BenchSort::Slug);
        assert!(all.len() > 2);
        assert!(all.windows(2).all(|w| w[0].slug <= w[1].slug));
        let grok = rows("grok 4.7", BenchSort::Intel);
        assert!(grok.iter().any(|row| row.slug == "grok-4-7-high"));
        assert!(grok.windows(2).all(|w| w[0].intel >= w[1].intel));
        assert!(rows("no-such-model-zz", BenchSort::Name).is_empty());
    }
}
