# MCP ServiceNow Server - Quick Fix & Reference

## ✅ Your Setup is Now Fixed!

### What Was Wrong:
1. **Same server trying to handle both Claude Code and Claude Desktop**
2. **Mismatched credentials between .env and Claude Desktop config**
3. **No clear separation between HTTP and STDIO modes**

### What I Fixed:
1. ✅ Separated HTTP server (port 3000) for Claude Code
2. ✅ Separated STDIO server for Claude Desktop
3. ✅ Updated Claude Desktop config with correct credentials
4. ✅ Added proper npm scripts for both modes
5. ✅ Created documentation for proper usage

## 🚀 Quick Commands

### For Claude Code (HTTP Mode):
```bash
# Start the HTTP server on port 3000
npm start

# Or with auto-reload
npm run dev
```

### For Claude Desktop (STDIO Mode):
```bash
# Already configured - just restart Claude Desktop
# It will auto-start the STDIO server when needed
```

### Testing:
```bash
# Test HTTP server
curl -H "Authorization: Bearer $HAPPY_MCP_API_TOKEN" http://localhost:3000/health

# Test STDIO server (manual)
node src/stdio-server.js
```

## 📁 Key Files

- **HTTP Server**: `src/server.js` (port 3000)
- **STDIO Server**: `src/stdio-server.js` (no port)
- **Config**: `.env` (environment variables)
- **Claude Desktop Config**: `~/Library/Application Support/Claude/claude_desktop_config.json`

## 🔧 If You Still Have Issues:

1. **Restart Claude Desktop** after config changes
2. **Kill stuck processes**: `pkill -f "node src"`
3. **Check logs**: Both servers output to console
4. **Verify .env file** has correct credentials

## 📊 Server Status

- **HTTP Server**: Running on port 3000 ✅
- **STDIO Server**: Ready for Claude Desktop ✅
- **161 ServiceNow tables** loaded and ready ✅
- **Environment**: Configured for dev276360 instance ✅

## 💡 Remember:

- **Claude Code** uses HTTP server (port 3000)
- **Claude Desktop** uses STDIO server (no port)
- They can run simultaneously without conflict
- Each has its own process and communication method