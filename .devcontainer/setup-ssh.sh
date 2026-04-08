#!/bin/bash
# Generate SSH keys for the devcontainer if they don't exist.
# Keys persist in the vector-ssh volume across rebuilds.
# Add the public key to GitHub: Settings → SSH keys → New SSH key
set -e

SSH_DIR="$HOME/.ssh"
KEY="$SSH_DIR/id_ed25519"

if [ ! -f "$KEY" ]; then
    echo "Generating SSH key for devcontainer..."
    chmod 700 "$SSH_DIR"
    ssh-keygen -t ed25519 -f "$KEY" -N "" -C "vector-devcontainer"
    echo ""
    echo "============================================"
    echo "Add this public key to GitHub:"
    echo "  https://github.com/settings/ssh/new"
    echo "============================================"
    echo ""
    cat "$KEY.pub"
    echo ""
else
    echo "SSH key exists in devcontainer volume."
fi

# Ensure correct permissions (volume mount can reset them)
chmod 700 "$SSH_DIR"
chmod 600 "$KEY" 2>/dev/null || true
chmod 644 "$KEY.pub" 2>/dev/null || true

# Configure git to use SSH for GitHub
git config --global url."git@github.com:".insteadOf "https://github.com/"
