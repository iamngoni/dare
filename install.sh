#!/usr/bin/env bash
set -euo pipefail

REPO="iamngoni/dare"
BINARY="dare"
INSTALL_DIR="/usr/local/bin"
SERVICE_NAME="dare-dashboard"

echo "🔨 dare.run installer"
echo "====================="

# Check for Rust
if ! command -v cargo &>/dev/null; then
    echo "❌ Rust/Cargo not found. Install from https://rustup.rs"
    exit 1
fi

# Clone or update
if [ -d "$HOME/dare" ]; then
    echo "📦 Updating existing repo..."
    cd "$HOME/dare"
    git pull --ff-only
else
    echo "📦 Cloning repo..."
    git clone "https://github.com/$REPO.git" "$HOME/dare"
    cd "$HOME/dare"
fi

# Build release
echo "🔨 Building release binary..."
cargo build --release

# Install binary
echo "📋 Installing to $INSTALL_DIR/$BINARY"
sudo cp "target/release/$BINARY" "$INSTALL_DIR/$BINARY"
sudo chmod +x "$INSTALL_DIR/$BINARY"

# Initialize dare in home directory
if [ ! -f "$HOME/dare/dare.toml" ]; then
    cd "$HOME/dare"
    dare init
fi

# Install systemd service
echo "🔧 Installing systemd service..."
sudo tee "/etc/systemd/system/$SERVICE_NAME.service" > /dev/null << EOF
[Unit]
Description=dare.run Dashboard
After=network.target

[Service]
Type=simple
User=$USER
WorkingDirectory=$HOME/dare
ExecStart=$INSTALL_DIR/$BINARY dashboard --no-open
Restart=on-failure
RestartSec=5
Environment=RUST_LOG=dare=info

[Install]
WantedBy=multi-user.target
EOF

sudo systemctl daemon-reload
sudo systemctl enable "$SERVICE_NAME"
sudo systemctl restart "$SERVICE_NAME"

echo ""
echo "✅ dare.run installed!"
echo "   Binary:    $INSTALL_DIR/$BINARY"
echo "   Dashboard: http://localhost:8895"
echo "   Service:   sudo systemctl status $SERVICE_NAME"
echo ""
echo "Usage:"
echo "   dare plan task.yaml        # Preview execution plan"
echo "   dare run task.yaml         # Execute tasks"
echo "   dare status                # Check run status"
echo "   dare dashboard --no-open   # Start dashboard manually"
