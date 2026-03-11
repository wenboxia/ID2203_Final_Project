#!/usr/bin/env bash
# Install Maelstrom and its dependencies.
# Maelstrom requires: Java (JDK 11+), Graphviz, gnuplot
set -euo pipefail

MAELSTROM_VERSION="0.2.3"
INSTALL_DIR="${MAELSTROM_DIR:-$(pwd)/maelstrom}"

echo "=== Installing Maelstrom v${MAELSTROM_VERSION} ==="

# Check for Java
if ! command -v java &> /dev/null; then
    echo "ERROR: Java (JDK 11+) is required. Install it first:"
    echo "  macOS:  brew install openjdk"
    echo "  Ubuntu: sudo apt install default-jdk"
    exit 1
fi

# Check for Graphviz
if ! command -v dot &> /dev/null; then
    echo "WARNING: Graphviz not found. Install for result visualization:"
    echo "  macOS:  brew install graphviz"
    echo "  Ubuntu: sudo apt install graphviz"
fi

# Check for gnuplot
if ! command -v gnuplot &> /dev/null; then
    echo "WARNING: gnuplot not found. Install for result visualization:"
    echo "  macOS:  brew install gnuplot"
    echo "  Ubuntu: sudo apt install gnuplot"
fi

# Download Maelstrom
if [ ! -d "${INSTALL_DIR}" ]; then
    echo "Downloading Maelstrom v${MAELSTROM_VERSION}..."
    ARCHIVE="maelstrom.tar.bz2"
    curl -LO "https://github.com/jepsen-io/maelstrom/releases/download/v${MAELSTROM_VERSION}/maelstrom.tar.bz2"
    tar xjf "${ARCHIVE}"
    mv maelstrom "${INSTALL_DIR}"
    rm -f "${ARCHIVE}"
    echo "Maelstrom installed to ${INSTALL_DIR}"
else
    echo "Maelstrom already installed at ${INSTALL_DIR}"
fi

echo ""
echo "=== Maelstrom installation complete ==="
echo "Binary: ${INSTALL_DIR}/maelstrom"
echo ""
echo "Next steps:"
echo "  1. Build the OmniPaxos Maelstrom node:  cargo build --release --bin maelstrom-node"
echo "  2. Run tests:                            ./scripts/run_tests.sh"
