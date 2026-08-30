//! Deterministic functional compatibility evaluation for Spark engine recipes.

use std::cmp::Reverse;

use super::wire::{
    CandidateEvaluationDocument, CandidateStatus, CompatibilityEvaluationDocument,
    FunctionalGateDocument, RecipeCatalogDocument, RecipeCompatibilityDocument, RecipeStatus,
    COMPATIBILITY_EVALUATION_SCHEMA,
};

const ENGINE_FAMILIES: [&str; 7] = [
    "tensorrt-llm",
    "vllm",
    "sglang",
    "llama.cpp",
    "nim",
    "mistral.rs",
    "candle-vllm",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvaluationError {
    Cancelled,
    NamedRecipeMissing,
}

pub struct EvaluationInput<'a> {
    pub id: &'a str,
    pub model_id: &'a str,
    pub repository: &'a str,
    pub commit: &'a str,
    pub objective: &'a str,
    pub named_recipe: Option<&'a str>,
    pub promote: bool,
    pub cancelled: bool,
    pub created_at: &'a str,
}

pub fn evaluate_catalog(
    catalog: &RecipeCatalogDocument,
    input: EvaluationInput<'_>,
) -> Result<CompatibilityEvaluationDocument, EvaluationError> {
    if input.cancelled {
        return Err(EvaluationError::Cancelled);
    }
    if input
        .named_recipe
        .is_some_and(|named| !catalog.recipes.iter().any(|recipe| recipe.id == named))
    {
        return Err(EvaluationError::NamedRecipeMissing);
    }
    let recipes = catalog
        .recipes
        .iter()
        .filter(|recipe| input.named_recipe.is_none_or(|named| recipe.id == named))
        .collect::<Vec<_>>();
    let mut candidates = recipes
        .iter()
        .map(|recipe| evaluate_recipe(recipe, input.objective))
        .collect::<Vec<_>>();
    for family in ENGINE_FAMILIES {
        if !recipes.iter().any(|recipe| recipe.engine == family) {
            candidates.push(uninstalled_family(family));
        }
    }
    candidates.sort_by(|left, right| {
        (left.engine_family.as_str(), left.recipe_id.as_deref())
            .cmp(&(right.engine_family.as_str(), right.recipe_id.as_deref()))
    });
    let winner = input.promote.then(|| select_winner(&candidates)).flatten();
    let (selected_recipe_id, selected_fingerprint) = winner
        .map(|index| {
            candidates[index].status = CandidateStatus::Selected;
            (
                candidates[index].recipe_id.clone(),
                candidates[index].fingerprint.clone(),
            )
        })
        .unwrap_or((None, None));
    let fallback_recipe_id = catalog
        .recipes
        .iter()
        .filter(|recipe| {
            recipe.compatible
                && recipe.engine == "vllm"
                && matches!(
                    recipe.status,
                    RecipeStatus::LocalVerified | RecipeStatus::UpstreamVerified
                )
        })
        .min_by_key(|recipe| (recipe.status != RecipeStatus::LocalVerified, &recipe.id))
        .map(|recipe| recipe.id.clone());
    Ok(CompatibilityEvaluationDocument {
        schema: COMPATIBILITY_EVALUATION_SCHEMA.into(),
        id: input.id.into(),
        model_id: input.model_id.into(),
        repository: input.repository.into(),
        commit: input.commit.into(),
        objective: input.objective.into(),
        selected_recipe_id,
        selected_fingerprint,
        fallback_recipe_id,
        candidates,
        created_at: input.created_at.into(),
        invalidated_reason: None,
    })
}

fn evaluate_recipe(
    recipe: &RecipeCompatibilityDocument,
    objective: &str,
) -> CandidateEvaluationDocument {
    let capability_ok = match objective {
        "agent" | "interactive" | "long-context" => recipe
            .capabilities
            .iter()
            .any(|value| value == "text_generation"),
        "retrieval" => recipe
            .capabilities
            .iter()
            .any(|value| value == "text_embeddings"),
        _ => false,
    };
    let local = recipe.status == RecipeStatus::LocalVerified;
    let gates = vec![
        gate(
            "exact_identity",
            recipe.compatible,
            "host, model, image, parser and recipe match",
        ),
        gate(
            "local_verification",
            local,
            "locally verified evidence is required",
        ),
        gate(
            "api_capability",
            capability_ok,
            "objective capability is recipe-declared",
        ),
        gate(
            "semantic_quality",
            !recipe.evidence.quality.trim().is_empty(),
            "recipe carries a semantic quality result",
        ),
        gate(
            "resource_safety",
            recipe.resources.startup_peak_bytes > 0 && recipe.resources.steady_peak_bytes > 0,
            "admission envelope is explicit",
        ),
        gate(
            "isolation",
            local,
            "local verification includes the recipe isolation boundary",
        ),
        gate(
            "durability",
            local,
            "local verification includes health and restart behavior",
        ),
    ];
    let unsupported = !recipe.compatible || recipe.status == RecipeStatus::Disabled;
    let eligible = !unsupported && gates.iter().all(|gate| gate.passed);
    CandidateEvaluationDocument {
        engine_family: recipe.engine.clone(),
        recipe_id: Some(recipe.id.clone()),
        fingerprint: Some(recipe.fingerprint.clone()),
        status: if unsupported {
            CandidateStatus::Unsupported
        } else if eligible {
            CandidateStatus::Eligible
        } else {
            CandidateStatus::Rejected
        },
        capability_tier: recipe.capabilities.len(),
        specialized_toggles: recipe.specialized_toggles,
        gates,
        reason: if unsupported {
            "installed recipe is not exactly compatible with this immutable model and host".into()
        } else if eligible {
            "all bounded functional gates passed".into()
        } else {
            "one or more functional gates failed".into()
        },
    }
}

fn gate(name: &str, passed: bool, detail: &str) -> FunctionalGateDocument {
    FunctionalGateDocument {
        name: name.into(),
        passed,
        detail: detail.into(),
    }
}

fn uninstalled_family(family: &str) -> CandidateEvaluationDocument {
    CandidateEvaluationDocument {
        engine_family: family.into(),
        recipe_id: None,
        fingerprint: None,
        status: CandidateStatus::Uninstalled,
        capability_tier: 0,
        specialized_toggles: 0,
        gates: Vec::new(),
        reason: "no frozen locally verified recipe is installed; no download was attempted".into(),
    }
}

fn select_winner(candidates: &[CandidateEvaluationDocument]) -> Option<usize> {
    candidates
        .iter()
        .enumerate()
        .filter(|(_, candidate)| candidate.status == CandidateStatus::Eligible)
        .min_by_key(|(_, candidate)| {
            (
                Reverse(candidate.capability_tier),
                candidate.specialized_toggles,
                candidate.recipe_id.as_deref().unwrap_or_default(),
            )
        })
        .map(|(index, _)| index)
}

#[cfg(test)]
pub fn apply_winner(
    catalog: &mut RecipeCatalogDocument,
    evaluation: &mut CompatibilityEvaluationDocument,
) {
    let Some(recipe_id) = evaluation.selected_recipe_id.as_deref() else {
        return;
    };
    let Some(fingerprint) = evaluation.selected_fingerprint.as_deref() else {
        return;
    };
    let Some(candidate) = catalog
        .recipes
        .iter()
        .find(|candidate| candidate.id == recipe_id && candidate.compatible)
    else {
        evaluation.invalidated_reason = Some("selected recipe is no longer compatible".into());
        return;
    };
    if candidate.fingerprint != fingerprint || catalog.objective != evaluation.objective {
        evaluation.invalidated_reason = Some("full recipe fingerprint or objective changed".into());
        return;
    }
    catalog.selection = Some(super::wire::RecipeSelectionDocument {
        recipe_id: recipe_id.into(),
        reason: super::wire::RecipeSelectionReason::TunedWinner,
        fingerprint: fingerprint.into(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ornith_catalog() -> RecipeCatalogDocument {
        crate::spark::recipe::RecipeCatalog::signed_for_test().query(
            &crate::spark::recipe::RecipeHost {
                architecture: "aarch64".into(),
                gpu_model: "NVIDIA GB10".into(),
                compute_capability: "12.1".into(),
                dgx_build: "7.5.0".into(),
                driver_version: "580.159.03".into(),
                toolkit_version: "1.19.0".into(),
                protected_fingerprint: format!(
                    "sha256:{}",
                    "7e42b88250e762400e91b902cfa1fcda6b4d1cc118eb6b91fd50716b41cf8510"
                ),
            },
            Some("ornith-ai/Ornith-1.5-9B"),
            Some("489cb97981b8654bcfcf30ce1f94ed1b62e07b53"),
            "agent",
            chrono::DateTime::parse_from_rfc3339("2026-08-25T00:00:00Z")
                .unwrap()
                .into(),
        )
    }

    fn candidate(id: &str, tier: usize, toggles: usize) -> CandidateEvaluationDocument {
        CandidateEvaluationDocument {
            engine_family: id.into(),
            recipe_id: Some(id.into()),
            fingerprint: Some(id.into()),
            status: CandidateStatus::Eligible,
            capability_tier: tier,
            specialized_toggles: toggles,
            gates: Vec::new(),
            reason: String::new(),
        }
    }

    #[test]
    fn functional_failures_cannot_rank_or_promote() {
        let mut failed = candidate("vllm", 9, 0);
        failed.status = CandidateStatus::Rejected;
        assert!(select_winner(&[failed]).is_none());
    }

    #[test]
    fn incompatible_installed_candidate_is_reported_as_unsupported() {
        let mut catalog = ornith_catalog();
        let recipe = catalog.recipes.first_mut().unwrap();
        recipe.compatible = false;
        assert_eq!(
            evaluate_recipe(recipe, "agent").status,
            CandidateStatus::Unsupported
        );
    }

    #[test]
    fn capability_simplicity_and_recipe_id_order_is_deterministic() {
        let candidates = [
            candidate("z", 3, 1),
            candidate("b", 3, 0),
            candidate("a", 3, 0),
        ];
        assert_eq!(select_winner(&candidates), Some(2));
    }

    #[test]
    fn cancellation_cleans_generation_without_promotion() {
        let catalog = RecipeCatalogDocument {
            schema: String::new(),
            catalog_sha256: String::new(),
            model_repository: None,
            model_commit: None,
            objective: "agent".into(),
            selection: None,
            recipes: Vec::new(),
        };
        let result = evaluate_catalog(
            &catalog,
            EvaluationInput {
                id: "e",
                model_id: "m",
                repository: "r",
                commit: "c",
                objective: "agent",
                named_recipe: None,
                promote: true,
                cancelled: true,
                created_at: "now",
            },
        );
        assert_eq!(result, Err(EvaluationError::Cancelled));
    }

    #[test]
    fn exact_winner_overrides_fallback_and_fingerprint_drift_retains_audit() {
        let mut catalog = ornith_catalog();
        let mut evaluation = evaluate_catalog(
            &catalog,
            EvaluationInput {
                id: "e",
                model_id: "m",
                repository: "ornith-ai/Ornith-1.5-9B",
                commit: "489cb97981b8654bcfcf30ce1f94ed1b62e07b53",
                objective: "agent",
                named_recipe: None,
                promote: true,
                cancelled: false,
                created_at: "now",
            },
        )
        .unwrap();
        apply_winner(&mut catalog, &mut evaluation);
        assert_eq!(
            catalog.selection.unwrap().reason,
            super::super::wire::RecipeSelectionReason::TunedWinner
        );

        let mut changed = ornith_catalog();
        evaluation.selected_fingerprint = Some(format!("sha256:{}", "0".repeat(64)));
        apply_winner(&mut changed, &mut evaluation);
        assert!(evaluation.invalidated_reason.is_some());
    }
}
