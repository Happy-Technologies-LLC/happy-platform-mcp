/**
 * Happy MCP Server - Multi-Instance Configuration Manager
 *
 * Copyright (c) 2025 Happy Technologies LLC
 * Licensed under the MIT License - see LICENSE file for details
 */

import fs from 'fs';
import path from 'path';
import { fileURLToPath } from 'url';

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);

export class ConfigManager {
  constructor() {
    this.configPath = path.join(__dirname, '../config/servicenow-instances.json');
    this.instances = null;
  }

  /**
   * Load instances from JSON config file
   */
  loadInstances() {
    if (this.instances) {
      return this.instances;
    }

    try {
      const configData = fs.readFileSync(this.configPath, 'utf8');
      const config = JSON.parse(configData);
      this.instances = config.instances;
      return this.instances;
    } catch (error) {
      // Fallback to .env if config file doesn't exist
      if (error.code === 'ENOENT') {
        console.warn('⚠️  servicenow-instances.json not found, falling back to .env');
        return this.loadFromEnv();
      }
      throw new Error(`Failed to load ServiceNow instances config: ${error.message}`);
    }
  }

  /**
   * Fallback: Load single instance from .env file (backward compatibility)
   */
  loadFromEnv() {
    if (!process.env.SERVICENOW_INSTANCE_URL || !process.env.SERVICENOW_USERNAME || !process.env.SERVICENOW_PASSWORD) {
      throw new Error('Missing ServiceNow credentials. Create config/servicenow-instances.json or set SERVICENOW_INSTANCE_URL, SERVICENOW_USERNAME, SERVICENOW_PASSWORD in .env');
    }

    const instance = {
      name: 'default',
      url: process.env.SERVICENOW_INSTANCE_URL,
      username: process.env.SERVICENOW_USERNAME,
      password: process.env.SERVICENOW_PASSWORD,
      default: true,
      description: 'Loaded from .env'
    };

    if (process.env.SERVICENOW_AUTH_TYPE === 'oauth') {
      instance.authType = 'oauth';
      instance.clientId = process.env.SERVICENOW_CLIENT_ID;
      instance.clientSecret = process.env.SERVICENOW_CLIENT_SECRET;
      instance.scope = process.env.SERVICENOW_OAUTH_SCOPE;
    }

    this.instances = [instance];

    return this.instances;
  }

  /**
   * Get instance by name
   * @param {string} name - Instance name
   * @returns {object} Instance configuration
   */
  getInstance(name) {
    const instances = this.loadInstances();
    const instance = instances.find(i => i.name === name);

    if (!instance) {
      throw new Error(`Instance '${name}' not found. Available instances: ${instances.map(i => i.name).join(', ')}`);
    }

    return instance;
  }

  /**
   * Get default instance
   * @returns {object} Default instance configuration
   */
  getDefaultInstance() {
    const instances = this.loadInstances();
    const defaultInstance = instances.find(i => i.default === true);

    if (!defaultInstance) {
      // If no default is set, use the first instance
      return instances[0];
    }

    return defaultInstance;
  }

  /**
   * Get instance by name or default if not specified
   * @param {string} name - Optional instance name
   * @returns {object} Instance configuration
   */
  getInstanceOrDefault(name = null) {
    if (name) {
      return this.getInstance(name);
    }

    // Check for SERVICENOW_INSTANCE env variable
    const envInstance = process.env.SERVICENOW_INSTANCE;
    if (envInstance) {
      return this.getInstance(envInstance);
    }

    return this.getDefaultInstance();
  }

  /**
   * List all available instances
   * @returns {Array} List of instance names and descriptions
   */
  listInstances() {
    const instances = this.loadInstances();
    return instances.map(i => ({
      name: i.name,
      url: i.url,
      default: i.default || false,
      description: i.description || ''
    }));
  }

  /**
   * Validate instance configuration
   * @param {object} instance - Instance configuration
   * @returns {boolean} True if valid
   */
  validateInstance(instance) {
    const required = ['name', 'url'];
    for (const field of required) {
      if (!instance[field]) {
        throw new Error(`Instance configuration missing required field: ${field}`);
      }
    }

    if (instance.authType === 'oauth') {
      if (!instance.clientId) {
        throw new Error(`OAuth instance '${instance.name}' missing required field: clientId`);
      }
      if (!instance.clientSecret) {
        throw new Error(`OAuth instance '${instance.name}' missing required field: clientSecret`);
      }
      // client_credentials grant doesn't need username/password
      if (instance.grantType !== 'client_credentials' && (!instance.username || !instance.password)) {
        throw new Error(`OAuth instance '${instance.name}' using password grant requires username and password`);
      }
    } else {
      // Basic auth requires username and password
      if (!instance.username || !instance.password) {
        throw new Error(`Instance '${instance.name}' missing required field: username or password`);
      }
    }

    return true;
  }
}

// Singleton instance
export const configManager = new ConfigManager();