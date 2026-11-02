#!/usr/bin/env node

/**
 * Refresh table metadata from a ServiceNow instance
 *
 * Reads sys_db_object and sys_dictionary for every table already listed in
 * src/config/comprehensive-table-definitions.json and updates those entries in
 * place. Tables the instance lacks, or that fail, keep their existing entry.
 * To add a table, add a stub entry ({ "priority": 2, "category": "..." }) first.
 *
 * Reads only metadata tables. Uses basic auth from the environment:
 *   SERVICENOW_INSTANCE_URL, SERVICENOW_USERNAME, SERVICENOW_PASSWORD
 *
 * Usage: npm run extract-metadata
 */

import dotenv from 'dotenv';
import { ServiceNowClient } from '../src/servicenow-client.js';
import fs from 'fs/promises';
import path from 'path';

dotenv.config();

const OUTPUT_PATH = path.resolve('src/config/comprehensive-table-definitions.json');

async function extractTableMetadata() {
  console.log('🔍 Connecting to ServiceNow instance...');

  const client = new ServiceNowClient(
    process.env.SERVICENOW_INSTANCE_URL,
    process.env.SERVICENOW_USERNAME,
    process.env.SERVICENOW_PASSWORD
  );

  const existing = JSON.parse(await fs.readFile(OUTPUT_PATH, 'utf-8'));
  const tablesToExtract = Object.keys(existing).filter((key) => key !== '_metadata');
  const tableMetadata = {
    ...existing,
    _metadata: {
      ...existing._metadata,
      total_tables: tablesToExtract.length,
      generated_on: new Date().toISOString()
    }
  };

  console.log(`📊 Refreshing ${tablesToExtract.length} tables from sys_db_object and sys_dictionary...`);
  let refreshed = 0;

  for (const tableName of tablesToExtract) {
    try {
      console.log(`\n📋 Processing table: ${tableName}`);

      // Get table definition from sys_db_object
      const tableDefResults = await client.getRecords('sys_db_object', {
        sysparm_query: `name=${tableName}`,
        sysparm_limit: 1
      });

      if (!tableDefResults || tableDefResults.length === 0) {
        console.log(`⚠️  Table ${tableName} not found in sys_db_object, skipping...`);
        continue;
      }

      const tableDef = tableDefResults[0];

      // Get dictionary entries for this table
      const dictEntries = await client.getRecords('sys_dictionary', {
        sysparm_query: `name=${tableName}^active=true`,
        sysparm_fields: 'element,column_label,mandatory,internal_type,reference,max_length',
        sysparm_limit: 200
      });

      // Separate required and common fields
      const requiredFields = [];
      const commonFields = [];

      dictEntries.forEach(entry => {
        if (entry.element && entry.element !== '') {
          if (entry.mandatory === 'true' || entry.mandatory === true) {
            requiredFields.push(entry.element);
          } else if (entry.internal_type !== 'collection') {
            // Include common fields but exclude collections
            commonFields.push(entry.element);
          }
        }
      });

      // Build metadata object
      const metadata = {
        label: tableDef.label || tableName,
        table: tableName,
        key_field: tableDef.number_ref ? 'number' : 'sys_id',
        display_field: tableDef.display_field || 'short_description',
        required_fields: requiredFields.length > 0 ? requiredFields : ['short_description'],
        common_fields: commonFields.slice(0, 30), // Limit to first 30 common fields
        operations: existing[tableName].operations || ['create', 'read', 'update', 'list'],
        description: existing[tableName].description || tableDef.sys_name || `${tableDef.label} table`,
        package: tableDef.sys_package?.display_value || 'Global',
        priority: existing[tableName].priority,
        category: existing[tableName].category,
        extends: tableDef.super_class?.display_value || null,
        is_extendable: tableDef.is_extendable === 'true'
      };

      tableMetadata[tableName] = metadata;
      refreshed += 1;
      console.log(`✅ Extracted metadata for ${tableName} (${requiredFields.length} required, ${commonFields.length} common fields)`);

    } catch (error) {
      console.error(`❌ Error processing table ${tableName}:`, error.message);
    }
  }

  await fs.writeFile(OUTPUT_PATH, `${JSON.stringify(tableMetadata, null, 2)}\n`);

  console.log(`\n✅ Refreshed ${refreshed} of ${tablesToExtract.length} tables`);
  console.log(`📄 Saved to: ${OUTPUT_PATH}`);
}

// Run the extraction
extractTableMetadata().catch(error => {
  console.error('❌ Fatal error:', error);
  process.exit(1);
});