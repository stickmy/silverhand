import { describe, expect, it } from 'vitest';
import fixture from './fixtures/runtime-delivery-presentation.json' with { type: 'json' };
import {
  composeRuntimeDeliveryPresentation,
  formatRuntimeDeliveryAttachmentSuffix,
  formatRuntimeDeliveryHeader,
  formatRuntimeDeliveryOptionsSuffix,
  type MessageAnswerDto,
  type MessageOptions,
  runtimeDeliveryInstructionSignal,
  type Reference,
  type RuntimeDeliveryFromKind,
  type RuntimeDeliveryHeaderInput,
  type RuntimeDeliveryInstructionSignal,
  type RuntimeDeliveryPresentation,
} from '../src/index.js';

describe('runtime delivery presentation authority', () => {
  const presentations = fixture.cases.map((testCase) => {
    const body =
      `${formatRuntimeDeliveryHeader(testCase.header as RuntimeDeliveryHeaderInput)}\n` +
      `${testCase.content}${formatRuntimeDeliveryAttachmentSuffix(testCase.attachments)}`;
    const instruction = runtimeDeliveryInstructionSignal({
      fromKind: testCase.fromKind as RuntimeDeliveryFromKind,
      references: testCase.references as Reference[],
      recipientAgentName: testCase.recipientAgentName,
    });
    return { body, instruction } satisfies RuntimeDeliveryPresentation;
  });

  it('keeps each sender, target, attachment and instruction fixture exact', () => {
    for (const [index, testCase] of fixture.cases.entries()) {
      expect(presentations[index], testCase.name).toEqual({
        body: testCase.expectedBody,
        instruction: testCase.expectedInstruction as RuntimeDeliveryInstructionSignal,
      });
    }
  });

  it('keeps the complete instruction signal set aligned with the Rust fixture', () => {
    const observed = fixture.instructionCases.map((testCase) => ({
      name: testCase.name,
      instruction: runtimeDeliveryInstructionSignal({
        fromKind: testCase.fromKind as RuntimeDeliveryFromKind,
        references: testCase.references as Reference[],
        recipientAgentName: testCase.recipientAgentName,
      }),
    }));
    expect(observed).toEqual(
      fixture.instructionCases.map((testCase) => ({
        name: testCase.name,
        instruction: testCase.expectedInstruction as RuntimeDeliveryInstructionSignal,
      })),
    );
    expect(new Set(observed.map((testCase) => testCase.instruction))).toEqual(
      new Set(['user_for_recipient', 'user_for_other', 'non_system_mention', 'system', 'other']),
    );
  });

  it('keeps the options block aligned with the Rust fixture', () => {
    for (const testCase of fixture.optionsCases) {
      expect(
        formatRuntimeDeliveryOptionsSuffix(
          testCase.messageId,
          testCase.options as MessageOptions,
          testCase.answer as MessageAnswerDto | null,
        ),
      ).toBe(testCase.expectedSuffix);
    }
  });

  it('keeps multi-row order and novel-remainder composition deterministic', () => {
    expect(composeRuntimeDeliveryPresentation(presentations, 'silverhand message send')).toBe(
      fixture.expectedBatch,
    );
    expect(
      composeRuntimeDeliveryPresentation(presentations.slice(1), 'silverhand message send'),
    ).toContain('You have 2 messages waiting since you were last active (oldest first):');
    expect(
      composeRuntimeDeliveryPresentation(presentations.slice(1), 'silverhand message send'),
    ).toContain('Respond as appropriate — call `silverhand message send` before stopping.');
  });
});
