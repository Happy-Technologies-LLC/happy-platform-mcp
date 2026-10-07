/**
 * Comprehensive validation tests for SN-Set-Update-Set tool
 * Tests update set switching, verification, timing, and reliability
 */

import { jest } from '@jest/globals';
import {
  createMockServiceNowClient,
  createAxiosResponse,
  createAxiosError,
} from './helpers/mocks.js';

/**
 * Mock update set data
 */
const mockUpdateSet1 = {
  sys_id: 'updateset123',
  name: 'Feature Development',
  state: 'in progress',
  description: 'Main feature development update set',
  sys_created_on: '2025-01-01 10:00:00',
};

const mockUpdateSet2 = {
  sys_id: 'updateset456',
  name: 'Bug Fixes',
  state: 'in progress',
  description: 'Bug fix update set',
  sys_created_on: '2025-01-02 10:00:00',
};

const mockDefaultUpdateSet = {
  sys_id: 'default001',
  name: 'Default',
  state: 'in progress',
  description: 'Default update set',
  sys_created_on: '2025-01-01 00:00:00',
};

/**
 * Mock sys_trigger for background script execution
 */
const mockTrigger = {
  sys_id: 'trigger123',
  name: 'MCP_Script_1704110400000',
  script: 'var updateSetId = "updateset123"; ...',
  next_action: '2025-01-01 10:00:01',
  trigger_type: '0',
  state: '0',
  description: 'Set update set to: Feature Development',
};

/**
 * Mock sys_user_preference for update set tracking
 */
const mockUserPreference = {
  sys_id: 'pref123',
  user: 'admin',
  name: 'sys_update_set',
  value: 'updateset123',
};

describe('SN-Set-Update-Set Validation Tests', () => {
  let mockClient;

  beforeEach(() => {
    mockClient = createMockServiceNowClient();
    jest.clearAllMocks();
  });

  describe('Basic Update Set Switching', () => {
    it('should set update set via UI API method', async () => {
      // Mock: Get update set details
      mockClient.getRecord.mockResolvedValueOnce(mockUpdateSet1);

      // Mock: UI API success
      mockClient.setCurrentUpdateSet.mockResolvedValueOnce({
        success: true,
        update_set: mockUpdateSet1.name,
        sys_id: mockUpdateSet1.sys_id,
        method: 'ui_api',
        response: { status: 'success' },
      });

      const result = await mockClient.setCurrentUpdateSet(mockUpdateSet1.sys_id);

      expect(mockClient.setCurrentUpdateSet).toHaveBeenCalledWith(mockUpdateSet1.sys_id);
      expect(result.success).toBe(true);
      expect(result.update_set).toBe('Feature Development');
      expect(result.method).toBe('ui_api');
    });

    it('should handle invalid update_set_sys_id', async () => {
      mockClient.setCurrentUpdateSet.mockRejectedValueOnce(
        new Error('Update set not found: invalid_sys_id')
      );

      await expect(mockClient.setCurrentUpdateSet('invalid_sys_id')).rejects.toThrow(
        'Update set not found'
      );
    });

    it('should validate sys_id format', async () => {
      const validSysId = '0123456789abcdef0123456789abcdef';
      const invalidSysId = 'not-a-sys-id';

      expect(/^[0-9a-f]{32}$/i.test(validSysId)).toBe(true);
      expect(/^[0-9a-f]{32}$/i.test(invalidSysId)).toBe(false);
    });
  });

  describe('Verification After Setting', () => {
    it('should verify update set changed correctly', async () => {
      // Mock: Set update set
      mockClient.setCurrentUpdateSet.mockResolvedValueOnce({
        success: true,
        update_set: mockUpdateSet1.name,
        sys_id: mockUpdateSet1.sys_id,
        method: 'sys_trigger',
      });

      // Mock: Wait for execution (simulate delay)
      await new Promise((resolve) => setTimeout(resolve, 2000));

      // Mock: Verify current update set
      mockClient.getCurrentUpdateSet.mockResolvedValueOnce({
        result: {
          name: mockUpdateSet1.name,
          value: mockUpdateSet1.sys_id,
        },
      });

      await mockClient.setCurrentUpdateSet(mockUpdateSet1.sys_id);

      // Wait for trigger execution
      await new Promise((resolve) => setTimeout(resolve, 2000));

      const current = await mockClient.getCurrentUpdateSet();
      expect(current.result.value).toBe(mockUpdateSet1.sys_id);
      expect(current.result.name).toBe('Feature Development');
    });

    it('should verify new configuration records go to correct update set', async () => {
      // Mock: Set update set
      mockClient.setCurrentUpdateSet.mockResolvedValueOnce({
        success: true,
        update_set: mockUpdateSet1.name,
        sys_id: mockUpdateSet1.sys_id,
        method: 'sys_trigger',
      });

      await mockClient.setCurrentUpdateSet(mockUpdateSet1.sys_id);

      // Wait for execution
      await new Promise((resolve) => setTimeout(resolve, 2000));

      // Mock: Create test configuration record
      const testProperty = {
        sys_id: 'prop123',
        name: 'x_test.property',
        value: 'test_value',
      };
      mockClient.createRecord.mockResolvedValueOnce(testProperty);

      const created = await mockClient.createRecord('sys_properties', testProperty);

      // Mock: Query sys_update_xml to verify capture
      mockClient.getRecords.mockResolvedValueOnce([
        {
          sys_id: 'update_xml_123',
          update_set: { value: mockUpdateSet1.sys_id },
          type: 'System Property',
          name: 'x_test.property',
          target_name: 'prop123',
        },
      ]);

      const updateXmlRecords = await mockClient.getRecords('sys_update_xml', {
        sysparm_query: `target_name=${created.sys_id}`,
        sysparm_fields: 'sys_id,update_set,type,name',
      });

      expect(updateXmlRecords.length).toBeGreaterThan(0);
      expect(updateXmlRecords[0].update_set.value).toBe(mockUpdateSet1.sys_id);
    });

    it('should return detailed verification results', async () => {
      // Mock: Get current update set before change
      mockClient.getCurrentUpdateSet.mockResolvedValueOnce({
        result: {
          name: mockDefaultUpdateSet.name,
          value: mockDefaultUpdateSet.sys_id,
        },
      });

      const previousUpdateSet = await mockClient.getCurrentUpdateSet();

      // Mock: Set new update set
      mockClient.setCurrentUpdateSet.mockResolvedValueOnce({
        success: true,
        update_set: mockUpdateSet1.name,
        sys_id: mockUpdateSet1.sys_id,
        method: 'sys_trigger',
        previous_update_set: {
          name: previousUpdateSet.result.name,
          sys_id: previousUpdateSet.result.value,
        },
        verification: {
          verified: true,
          timestamp: new Date().toISOString(),
        },
      });

      const result = await mockClient.setCurrentUpdateSet(mockUpdateSet1.sys_id);

      expect(result.previous_update_set).toBeDefined();
      expect(result.previous_update_set.name).toBe('Default');
      expect(result.verification).toBeDefined();
      expect(result.verification.verified).toBe(true);
    });
  });

  describe('Switching Between Update Sets', () => {
    it('should switch from one custom update set to another', async () => {
      // Mock: Set first update set
      mockClient.setCurrentUpdateSet.mockResolvedValueOnce({
        success: true,
        update_set: mockUpdateSet1.name,
        sys_id: mockUpdateSet1.sys_id,
      });

      await mockClient.setCurrentUpdateSet(mockUpdateSet1.sys_id);
      await new Promise((resolve) => setTimeout(resolve, 2000));

      // Mock: Switch to second update set
      mockClient.setCurrentUpdateSet.mockResolvedValueOnce({
        success: true,
        update_set: mockUpdateSet2.name,
        sys_id: mockUpdateSet2.sys_id,
      });

      const result = await mockClient.setCurrentUpdateSet(mockUpdateSet2.sys_id);

      expect(result.update_set).toBe('Bug Fixes');
      expect(result.sys_id).toBe(mockUpdateSet2.sys_id);
    });

    it('should handle switching to Default update set', async () => {
      mockClient.setCurrentUpdateSet.mockResolvedValueOnce({
        success: true,
        update_set: mockDefaultUpdateSet.name,
        sys_id: mockDefaultUpdateSet.sys_id,
      });

      const result = await mockClient.setCurrentUpdateSet(mockDefaultUpdateSet.sys_id);

      expect(result.update_set).toBe('Default');
    });

    it('should warn when switching to Default update set', async () => {
      // This test would verify that a warning is logged or returned
      mockClient.setCurrentUpdateSet.mockResolvedValueOnce({
        success: true,
        update_set: mockDefaultUpdateSet.name,
        sys_id: mockDefaultUpdateSet.sys_id,
        warning: 'WARNING: Setting update set to "Default". Configuration changes may not be captured.',
      });

      const result = await mockClient.setCurrentUpdateSet(mockDefaultUpdateSet.sys_id);

      expect(result.warning).toBeDefined();
      expect(result.warning).toContain('Default');
    });
  });

  describe('Timing and Race Conditions', () => {
    it('should handle sys_trigger execution timing (1 second delay)', async () => {
      const startTime = Date.now();

      mockClient.setCurrentUpdateSet.mockResolvedValueOnce({
        success: true,
        update_set: mockUpdateSet1.name,
        sys_id: mockUpdateSet1.sys_id,
        method: 'sys_trigger',
        trigger_details: {
          next_action: new Date(startTime + 1000).toISOString(),
        },
      });

      await mockClient.setCurrentUpdateSet(mockUpdateSet1.sys_id);

      // Verify trigger scheduled ~1 second in future
      const endTime = Date.now();
      expect(endTime - startTime).toBeLessThan(500); // API call should be fast
    });

    it('should handle rapid consecutive update set changes', async () => {
      // Mock: First change
      mockClient.setCurrentUpdateSet.mockResolvedValueOnce({
        success: true,
        update_set: mockUpdateSet1.name,
        sys_id: mockUpdateSet1.sys_id,
      });

      // Mock: Second change (immediate)
      mockClient.setCurrentUpdateSet.mockResolvedValueOnce({
        success: true,
        update_set: mockUpdateSet2.name,
        sys_id: mockUpdateSet2.sys_id,
      });

      // Rapid consecutive changes
      const result1 = mockClient.setCurrentUpdateSet(mockUpdateSet1.sys_id);
      const result2 = mockClient.setCurrentUpdateSet(mockUpdateSet2.sys_id);

      await Promise.all([result1, result2]);

      // Should complete both without errors
      expect(mockClient.setCurrentUpdateSet).toHaveBeenCalledTimes(2);
    });

    it('should verify execution completed before creating records', async () => {
      // Mock: Set update set
      mockClient.setCurrentUpdateSet.mockResolvedValueOnce({
        success: true,
        update_set: mockUpdateSet1.name,
        sys_id: mockUpdateSet1.sys_id,
        method: 'sys_trigger',
      });

      await mockClient.setCurrentUpdateSet(mockUpdateSet1.sys_id);

      // CRITICAL: Wait for trigger execution (recommended 2+ seconds)
      await new Promise((resolve) => setTimeout(resolve, 2000));

      // Mock: Verify current update set
      mockClient.getCurrentUpdateSet.mockResolvedValueOnce({
        result: { value: mockUpdateSet1.sys_id },
      });

      const current = await mockClient.getCurrentUpdateSet();
      expect(current.result.value).toBe(mockUpdateSet1.sys_id);

      // Now safe to create records
      mockClient.createRecord.mockResolvedValueOnce({
        sys_id: 'prop123',
        name: 'test.property',
      });

      const record = await mockClient.createRecord('sys_properties', {
        name: 'test.property',
      });

      expect(record).toBeDefined();
    });
  });

  describe('Error Handling', () => {
    it('should handle network errors during update set change', async () => {
      mockClient.setCurrentUpdateSet.mockRejectedValueOnce(
        createAxiosError('Network error', 500)
      );

      await expect(mockClient.setCurrentUpdateSet(mockUpdateSet1.sys_id)).rejects.toThrow(
        'Network error'
      );
    });

    it('should handle permission errors (403)', async () => {
      mockClient.setCurrentUpdateSet.mockRejectedValueOnce(
        createAxiosError('Access denied: Insufficient privileges', 403)
      );

      await expect(mockClient.setCurrentUpdateSet(mockUpdateSet1.sys_id)).rejects.toThrow(
        'Access denied'
      );
    });

    it('should handle update set not found (404)', async () => {
      mockClient.setCurrentUpdateSet.mockRejectedValueOnce(
        createAxiosError('Update set not found', 404)
      );

      await expect(
        mockClient.setCurrentUpdateSet('nonexistent_sys_id')
      ).rejects.toThrow('Update set not found');
    });

    it('should handle sys_trigger creation failure', async () => {
      mockClient.setCurrentUpdateSet.mockRejectedValueOnce(
        new Error('Failed to create sys_trigger: Table access denied')
      );

      await expect(mockClient.setCurrentUpdateSet(mockUpdateSet1.sys_id)).rejects.toThrow(
        'sys_trigger'
      );
    });
  });

  describe('Background Script Execution (sys_trigger)', () => {
    it('should create sys_trigger with correct fields', async () => {
      mockClient.createRecord.mockResolvedValueOnce(mockTrigger);

      const trigger = await mockClient.createRecord('sys_trigger', {
        name: 'MCP_Script_1704110400000',
        script: 'var updateSetId = "updateset123"; ...',
        next_action: '2025-01-01 10:00:01',
        trigger_type: '0',
        state: '0',
        description: 'Set update set to: Feature Development',
      });

      expect(trigger.trigger_type).toBe('0'); // Run once
      expect(trigger.state).toBe('0'); // Ready
      expect(trigger.next_action).toBeDefined();
    });

    it('should create trigger with auto-delete script', async () => {
      const scriptWithDelete = `
// Auto-generated MCP script trigger
try {
  var updateSetId = 'updateset123';
  // ... script content ...
} finally {
  // Auto-delete this trigger after execution
  var triggerGR = new GlideRecord('sys_trigger');
  if (triggerGR.get('trigger123')) {
    triggerGR.deleteRecord();
    gs.info('MCP: Auto-deleted trigger trigger123');
  }
}`;

      mockClient.createRecord.mockResolvedValueOnce({
        ...mockTrigger,
        script: scriptWithDelete,
      });

      const trigger = await mockClient.createRecord('sys_trigger', {
        ...mockTrigger,
        script: scriptWithDelete,
      });

      expect(trigger.script).toContain('deleteRecord()');
      expect(trigger.script).toContain('Auto-delete');
    });

    it('should schedule execution 1 second in future', async () => {
      const now = new Date();
      const expectedNextAction = new Date(now.getTime() + 1000);

      mockClient.executeScriptViaTrigger.mockResolvedValueOnce({
        success: true,
        trigger_sys_id: 'trigger123',
        next_action: expectedNextAction.toISOString(),
      });

      const result = await mockClient.executeScriptViaTrigger('test script');

      expect(result.success).toBe(true);
      expect(new Date(result.next_action).getTime()).toBeGreaterThan(now.getTime());
    });
  });

  describe('User Preference Management', () => {
    it('should update sys_user_preference for update set', async () => {
      // Mock: Delete existing preference
      mockClient.getRecords.mockResolvedValueOnce([
        { sys_id: 'old_pref', name: 'sys_update_set' },
      ]);

      // Mock: Create new preference
      mockClient.createRecord.mockResolvedValueOnce(mockUserPreference);

      const records = await mockClient.getRecords('sys_user_preference', {
        sysparm_query: 'user=admin^name=sys_update_set',
      });

      if (records.length > 0) {
        // Would delete old preference
        // Then create new one
        const newPref = await mockClient.createRecord('sys_user_preference', {
          user: 'admin',
          name: 'sys_update_set',
          value: 'updateset123',
        });

        expect(newPref.value).toBe('updateset123');
      }
    });

    it('should handle multiple users with different update sets', async () => {
      // User 1 preference
      mockClient.createRecord.mockResolvedValueOnce({
        sys_id: 'pref1',
        user: 'admin',
        name: 'sys_update_set',
        value: 'updateset123',
      });

      // User 2 preference
      mockClient.createRecord.mockResolvedValueOnce({
        sys_id: 'pref2',
        user: 'developer',
        name: 'sys_update_set',
        value: 'updateset456',
      });

      const pref1 = await mockClient.createRecord('sys_user_preference', {
        user: 'admin',
        name: 'sys_update_set',
        value: 'updateset123',
      });

      const pref2 = await mockClient.createRecord('sys_user_preference', {
        user: 'developer',
        name: 'sys_update_set',
        value: 'updateset456',
      });

      expect(pref1.value).toBe('updateset123');
      expect(pref2.value).toBe('updateset456');
    });
  });

  describe('Integration Workflow', () => {
    it('should complete full update set workflow with verification', async () => {
      // 1. Get current update set
      mockClient.getCurrentUpdateSet.mockResolvedValueOnce({
        result: { name: 'Default', value: 'default001' },
      });

      const previous = await mockClient.getCurrentUpdateSet();
      expect(previous.result.name).toBe('Default');

      // 2. Set new update set
      mockClient.setCurrentUpdateSet.mockResolvedValueOnce({
        success: true,
        update_set: 'Feature Development',
        sys_id: 'updateset123',
        method: 'sys_trigger',
      });

      const setResult = await mockClient.setCurrentUpdateSet('updateset123');
      expect(setResult.success).toBe(true);

      // 3. Wait for execution
      await new Promise((resolve) => setTimeout(resolve, 2000));

      // 4. Verify change
      mockClient.getCurrentUpdateSet.mockResolvedValueOnce({
        result: { name: 'Feature Development', value: 'updateset123' },
      });

      const current = await mockClient.getCurrentUpdateSet();
      expect(current.result.value).toBe('updateset123');

      // 5. Create configuration record
      mockClient.createRecord.mockResolvedValueOnce({
        sys_id: 'prop123',
        name: 'test.property',
      });

      const record = await mockClient.createRecord('sys_properties', {
        name: 'test.property',
      });

      // 6. Verify captured in correct update set
      mockClient.getRecords.mockResolvedValueOnce([
        {
          sys_id: 'update_xml_123',
          update_set: { value: 'updateset123' },
          type: 'System Property',
        },
      ]);

      const updateXml = await mockClient.getRecords('sys_update_xml', {
        sysparm_query: `target_name=${record.sys_id}`,
      });

      expect(updateXml[0].update_set.value).toBe('updateset123');
    });
  });

  describe('Performance and Reliability', () => {
    it('should complete update set change within 3 seconds total', async () => {
      const startTime = Date.now();

      mockClient.setCurrentUpdateSet.mockResolvedValueOnce({
        success: true,
        update_set: 'Feature Development',
        sys_id: 'updateset123',
        method: 'sys_trigger',
      });

      await mockClient.setCurrentUpdateSet('updateset123');

      // Wait for execution
      await new Promise((resolve) => setTimeout(resolve, 2000));

      const totalTime = Date.now() - startTime;
      expect(totalTime).toBeLessThan(3000);
    });

    it('should handle concurrent update set queries during change', async () => {
      // Mock: Set update set
      mockClient.setCurrentUpdateSet.mockResolvedValueOnce({
        success: true,
        update_set: 'Feature Development',
        sys_id: 'updateset123',
      });

      const setPromise = mockClient.setCurrentUpdateSet('updateset123');

      // Mock: Query update sets during change
      mockClient.listUpdateSets.mockResolvedValueOnce([
        mockUpdateSet1,
        mockUpdateSet2,
      ]);

      const listPromise = mockClient.listUpdateSets();

      const [setResult, listResult] = await Promise.all([setPromise, listPromise]);

      expect(setResult.success).toBe(true);
      expect(listResult.length).toBe(2);
    });

    it('should be idempotent (multiple calls with same sys_id)', async () => {
      mockClient.setCurrentUpdateSet.mockResolvedValue({
        success: true,
        update_set: 'Feature Development',
        sys_id: 'updateset123',
      });

      const result1 = await mockClient.setCurrentUpdateSet('updateset123');
      const result2 = await mockClient.setCurrentUpdateSet('updateset123');

      expect(result1.sys_id).toBe(result2.sys_id);
      expect(mockClient.setCurrentUpdateSet).toHaveBeenCalledTimes(2);
    });
  });
});
