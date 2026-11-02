# Claude Desktop Setup Guide

**Complete guide for integrating ServiceNow MCP Server with Claude Desktop**

---

## ✅ Current Working Configuration

Your Claude Desktop is configured with **Option A: Multi-Instance with Runtime Switching** ⭐

**Config File:** `~/Library/Application Support/Claude/claude_desktop_config.json`

```json
{
  "mcpServers": {
    "servicenow-nodejs": {
      "command": "node",
      "args": ["/Users/nczitzer/WebstormProjects/mcp-servicenow-nodejs/src/stdio-server.js"],
      "cwd": "/Users/nczitzer/WebstormProjects/mcp-servicenow-nodejs"
    }
  }
}
```

**Instance Configuration:** `config/servicenow-instances.json`

The server automatically loads your instances from the config file. You can switch between instances during your conversation without restarting!

---

## 🔧 Setup Steps (If Not Already Configured)

### 1. Open Claude Desktop Config

```bash
# Open in your default editor
open ~/Library/Application\ Support/Claude/claude_desktop_config.json

# Or use nano
nano ~/Library/Application\ Support/Claude/claude_desktop_config.json
```

### 2. Choose Configuration Method

**You have TWO options - choose ONE:**

#### Option A: Multi-Instance with Runtime Switching (Recommended) ⭐

**Step 1:** Create `config/servicenow-instances.json`:
```bash
cd /Users/YOUR_USERNAME/WebstormProjects/mcp-servicenow-nodejs
nano config/servicenow-instances.json
```

```json
{
  "instances": [
    {
      "name": "dev",
      "url": "https://dev123456.service-now.com",
      "username": "admin",
      "password": "password",
      "default": true
    }
  ]
}
```

**Step 2:** Add to Claude Desktop config (NO env section):
```json
{
  "mcpServers": {
    "servicenow-nodejs": {
      "command": "node",
      "args": ["/Users/YOUR_USERNAME/WebstormProjects/mcp-servicenow-nodejs/src/stdio-server.js"],
      "cwd": "/Users/YOUR_USERNAME/WebstormProjects/mcp-servicenow-nodejs"
    }
  }
}
```

**Benefits:** ✅ One config file, ✅ Runtime switching, ✅ Easy to add instances

---

#### Option B: Single Instance (Simple)

Add to Claude Desktop config (WITH env section):
```json
{
  "mcpServers": {
    "servicenow-nodejs": {
      "command": "node",
      "args": ["/Users/YOUR_USERNAME/WebstormProjects/mcp-servicenow-nodejs/src/stdio-server.js"],
      "cwd": "/Users/YOUR_USERNAME/WebstormProjects/mcp-servicenow-nodejs",
      "env": {
        "SERVICENOW_INSTANCE_URL": "https://your-instance.service-now.com",
        "SERVICENOW_USERNAME": "your-username",
        "SERVICENOW_PASSWORD": "your-password"
      }
    }
  }
}
```

**Benefits:** ✅ Simple, ✅ Everything in one file

**Limitations:** ❌ No instance switching, ❌ Restart required to change instances

---

**Important:** Replace `YOUR_USERNAME` with your actual username

### 3. Restart Claude Desktop

**Complete restart required:**
1. Quit Claude Desktop (⌘Q)
2. Wait 2-3 seconds
3. Reopen Claude Desktop

---

## 🌐 Multi-Instance Setup with Runtime Switching (Recommended!)

**Feature:** Switch between instances **during your conversation** without restarting!

To enable this, use the JSON config file instead of ENV variables:

### 1. Create Config File

```bash
# Copy example config
cp config/servicenow-instances.json.example config/servicenow-instances.json

# Edit with your instances
nano config/servicenow-instances.json
```

### 2. Configure Instances

```json
{
  "instances": [
    {
      "name": "dev",
      "url": "https://dev123456.service-now.com",
      "username": "admin",
      "password": "password",
      "default": true
    },
    {
      "name": "prod",
      "url": "https://prod789012.service-now.com",
      "username": "integration_user",
      "password": "password"
    }
  ]
}
```

### 3. Remove ENV Variables from Claude Config

When using multi-instance config, remove the `env` section:

```json
{
  "mcpServers": {
    "servicenow-nodejs": {
      "command": "node",
      "args": ["/Users/YOUR_USERNAME/WebstormProjects/mcp-servicenow-nodejs/src/stdio-server.js"],
      "cwd": "/Users/YOUR_USERNAME/WebstormProjects/mcp-servicenow-nodejs"
    }
  }
}
```

The server will automatically load from `config/servicenow-instances.json`.

### 4. Use Instance Switching in Claude Desktop

**During your conversation**, you can now switch instances:

```
You: "Switch to prod instance"
Claude: Uses SN-Set-Instance({ instance_name: "prod" })
       ✅ Switched successfully!

You: "List incidents"
Claude: Uses SN-List-Incidents()
       → Queries PROD instance

You: "Switch back to dev"
Claude: Uses SN-Set-Instance({ instance_name: "dev" })
       ✅ Switched to dev

You: "Create a test incident"
Claude: Uses SN-Create-Incident()
       → Creates in DEV instance
```

**Benefits:**
- ✅ No restart required
- ✅ Switch anytime during conversation
- ✅ All tools automatically use current instance
- ✅ Can compare data across instances
- ✅ Safe multi-environment workflows

---

## ✅ Verification

### 1. Check Tools Are Loaded

After restarting Claude Desktop, you should see:
- "ServiceNow" as an available context
- 60 MCP tools available
- Tools like `SN-Query-Table`, `SN-Create-Record`, etc.

### 2. Test a Tool

Try this in Claude Desktop:

```
List 5 incidents from ServiceNow
```

Claude should use `SN-List-Incidents` automatically.

### 3. Test Instance Switching (Multi-Instance Only)

If you have multiple instances configured:

```
Show me available ServiceNow instances
```

Claude uses: `SN-Set-Instance()` (no params = list instances)

```
Switch to prod instance
```

Claude uses: `SN-Set-Instance({ instance_name: "prod" })`

```
What instance am I on?
```

Claude uses: `SN-Get-Current-Instance()`

**All subsequent operations will use the switched instance!**

### 3. Check Server Logs

If issues occur, check the server logs:

```bash
# The stdio server logs to stderr, which Claude Desktop captures
# Check Claude Desktop's developer console:
# Help → Developer → Open Developer Console
```

---

## 🔍 Troubleshooting

### Claude Desktop Doesn't Show ServiceNow Tools

**Cause:** Config file has errors or Claude hasn't restarted properly

**Fix:**
1. Validate JSON syntax: `cat ~/Library/Application\ Support/Claude/claude_desktop_config.json | jq`
2. Kill all Claude processes: `pkill -9 Claude`
3. Restart Claude Desktop
4. Wait 5-10 seconds for tools to load

---

### "Command not found" Error

**Cause:** Node.js not in PATH or wrong path to stdio-server.js

**Fix:**
1. Verify Node.js is installed: `which node`
2. Verify path exists: `ls /Users/nczitzer/WebstormProjects/mcp-servicenow-nodejs/src/stdio-server.js`
3. Use full path to node: `/usr/local/bin/node` (or wherever `which node` shows)

```json
{
  "command": "/usr/local/bin/node",
  "args": ["/full/path/to/src/stdio-server.js"]
}
```

---

### Authentication Errors

**Cause:** Wrong credentials or instance URL

**Fix:**
1. Run the safe local CLI check:
   ```bash
   happy-platform-mcp instance test <instance-name>
   ```
   For a source checkout, use `node src/cli.js instance test <instance-name>`
   (or `npm link` and then the global command).
2. If testing the ServiceNow API directly, use an approved secret manager or
   an interactive API client; do not put credentials in shell history,
   command arguments, or documentation.
3. Verify the instance URL, username, OAuth metadata, and keychain credential
   reference.

---

### Tools Work But Return Errors

**Cause:** ServiceNow permissions or instance issues

**Fix:**
1. Check user has required roles (admin, rest_api_explorer)
2. Verify instance is accessible
3. Check ServiceNow system logs for API errors

---

## 📋 Comparison: Claude Desktop vs Claude Code

| Feature | Claude Desktop | Claude Code |
|---------|---------------|-------------|
| **Transport** | STDIO | HTTP (SSE) |
| **Configuration** | JSON config file | Not needed (uses HTTP) |
| **Server Start** | Auto-starts on demand | Manual (`npm run dev`) |
| **Port** | None (stdio) | 3000 (HTTP) |
| **Credentials** | In config file | In `.env` or `config/servicenow-instances.json` |
| **Multi-Instance** | Via config file | Via HTTP endpoint |

Both can run **simultaneously** without conflict!

---

## 🚀 Quick Reference

### Config File Location
```bash
~/Library/Application Support/Claude/claude_desktop_config.json
```

### Edit Config
```bash
nano ~/Library/Application\ Support/Claude/claude_desktop_config.json
```

### Restart Claude
```bash
pkill -9 Claude && open -a Claude
```

### Validate JSON
```bash
cat ~/Library/Application\ Support/Claude/claude_desktop_config.json | jq
```

### Test Server Manually
```bash
cd /Users/nczitzer/WebstormProjects/mcp-servicenow-nodejs
node src/stdio-server.js
# Should wait for MCP JSON-RPC messages on stdin
```

---

## 📚 Additional Resources

- **Main README:** `../README.md`
- **API Reference:** `API_REFERENCE.md`
- **Multi-Instance Config:** `MULTI_INSTANCE_CONFIGURATION.md`
- **Troubleshooting:** `403_TROUBLESHOOTING.md`