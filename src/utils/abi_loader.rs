// src/utils/abi_loader.rs

use anyhow::{anyhow, Context, Result};
use ethers::abi::Abi;
use serde::Deserialize; // Necessário para desserialização flexível
use std::{fs, path::Path};

// Estruturas auxiliares para desserializar ABIs aninhados.
// Exemplo: { "abi": [...] }
#[derive(Deserialize)]
struct AbiWrapper {
    abi: Option<Abi>,
    // Adiciona outros campos comuns, como 'output', que pode conter o ABI em alguns compiladores
    output: Option<AbiOutput>,
}

#[derive(Deserialize)]
struct AbiOutput {
    abi: Option<Abi>,
}

/// Carrega um arquivo ABI JSON da pasta 'abi/' do projeto.
/// Tenta desserializar diretamente como ABI (array), ou como um objeto envolto.
pub fn load_abi(filename: &str) -> Result<Abi> {
    let abi_path = Path::new("abi").join(filename);

    let abi_json = fs::read_to_string(&abi_path).with_context(|| {
        format!(
            "Falha ao ler o arquivo ABI. Verifique se '{}' existe em sua pasta 'abi/'.",
            abi_path.display()
        )
    })?;

    // Tenta 1: Desserializar diretamente como um array de ABI padrão.
    if let Ok(abi) = serde_json::from_str::<Abi>(&abi_json) {
        return Ok(abi);
    }

    // Tenta 2: Desserializar como um objeto envolto (Ex: Hardhat, Solc, etc.).
    if let Ok(wrapper) = serde_json::from_str::<AbiWrapper>(&abi_json) {
        if let Some(abi) = wrapper.abi {
            return Ok(abi);
        }
        if let Some(output) = wrapper.output {
            if let Some(abi) = output.abi {
                return Ok(abi);
            }
        }
    }

    // Se falhar, retorna o erro com contexto.
    Err(anyhow!(
        "Falha ao desserializar ABI para '{}'. O formato JSON está incorreto. Esperado um array ABI ([...]) ou um objeto com a chave 'abi'.",
        filename
    ))
}
