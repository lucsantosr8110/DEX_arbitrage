@echo off
echo 🔧 Iniciando Flashloan Bot - Modo Producao Baixo Risco
echo 📅 %date% %time%
echo.

taskkill /f /im flashloan-bot.exe 2>nul
timeout /t 3 /nobreak >nul

echo 🏗️  Compilando versao de producao...
cargo build --release
if %errorlevel% neq 0 (
    echo ❌ Erro na compilacao
    pause
    exit /b 1
)

echo 🚀 Iniciando bot de producao...
echo 📊 Config: Baixo Risco | Lucro Min: $0.25 | Max Gas: 80 Gwei
echo.

target\release\flashloan-bot.exe

echo.
echo 📉 Bot finalizado as %time%
pause