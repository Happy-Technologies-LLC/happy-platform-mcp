#!/usr/bin/env node

/**
 * SSE Keepalive Test - Demonstrates the heartbeat mechanism
 *
 * This simple server shows how the SSE keepalive works:
 * - Sends keepalive comments every 15 seconds
 * - Disables timeouts
 * - Sets proper headers
 *
 * It is a standalone, unauthenticated demo (not the Happy MCP HTTP server),
 * so it only listens on loopback. The real server requires
 * `Authorization: Bearer $HAPPY_MCP_API_TOKEN` on every request.
 */

import express from 'express';

const app = express();
const SSE_KEEPALIVE_INTERVAL = parseInt(process.env.SSE_KEEPALIVE_INTERVAL || '15000', 10);

app.get('/sse-test', (req, res) => {
  // SSE-specific headers
  res.setHeader('Content-Type', 'text/event-stream');
  res.setHeader('Cache-Control', 'no-cache, no-transform');
  res.setHeader('X-Accel-Buffering', 'no');
  res.setHeader('Connection', 'keep-alive');

  // Disable timeouts
  req.setTimeout(0);
  res.setTimeout(0);

  console.log('✅ Client connected');

  // Send initial message
  res.write('data: Connected! Keepalive will occur every ' + (SSE_KEEPALIVE_INTERVAL / 1000) + ' seconds\n\n');

  let keepaliveCount = 0;

  // Set up keepalive heartbeat
  const keepaliveInterval = setInterval(() => {
    try {
      keepaliveCount++;
      // Send SSE comment (invisible to client)
      res.write(': keepalive\n\n');
      console.log(`💓 Keepalive sent (${keepaliveCount})`);
    } catch (error) {
      console.error('❌ Keepalive failed:', error.message);
      clearInterval(keepaliveInterval);
    }
  }, SSE_KEEPALIVE_INTERVAL);

  // Clean up on disconnect
  req.on('close', () => {
    clearInterval(keepaliveInterval);
    console.log(`🔌 Client disconnected after ${keepaliveCount} keepalives`);
  });

  req.on('error', (error) => {
    console.error('❌ Request error:', error);
    clearInterval(keepaliveInterval);
  });
});

const PORT = process.env.PORT || 3001;
app.listen(PORT, '127.0.0.1', () => {
  console.log(`🚀 SSE Test Server listening on http://127.0.0.1:${PORT}`);
  console.log(`💓 Keepalive interval: ${SSE_KEEPALIVE_INTERVAL}ms`);
  console.log('\n📋 Test commands:');
  console.log(`   curl -N http://127.0.0.1:${PORT}/sse-test`);
  console.log(`   # Watch for keepalive comments every ${SSE_KEEPALIVE_INTERVAL / 1000} seconds\n`);
});
