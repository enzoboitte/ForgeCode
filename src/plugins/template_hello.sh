#!/bin/sh
# Example module command: plain output goes to the Output panel,
# `@forge {json}` lines are actions executed by the IDE.
echo "Workspace: $FORGE_WORKSPACE"
echo "Current file: $FORGE_FILE ($FORGE_LANGUAGE)"
echo '@forge {"action":"notify","message":"Hello from a ForgeCode module!"}'
