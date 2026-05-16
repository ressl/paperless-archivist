import { describe, expect, it } from 'vitest';
import {
  buildEditedReviewPatch,
  reviewEditStateFromPatch,
  type ReviewEditState,
  type ReviewPatchRecord
} from './Reviews';

describe('reviewEditStateFromPatch', () => {
  it('returns an empty state when the patch is null', () => {
    expect(reviewEditStateFromPatch(null)).toEqual({});
  });

  it('lifts only the recognised editable fields out of the patch', () => {
    const patch: ReviewPatchRecord = {
      title: 'Invoice 4711',
      correspondent: 17,
      document_type: 9,
      created: '2026-05-01',
      tags: ['ignored']
    };
    expect(reviewEditStateFromPatch(patch)).toEqual({
      title: 'Invoice 4711',
      correspondent: '17',
      document_type: '9',
      created: '2026-05-01'
    });
  });

  it('preserves explicit null values as empty strings so the user can re-edit them', () => {
    const patch: ReviewPatchRecord = {
      title: null,
      correspondent: null
    } as ReviewPatchRecord;
    expect(reviewEditStateFromPatch(patch)).toEqual({
      title: '',
      correspondent: ''
    });
  });
});

describe('buildEditedReviewPatch', () => {
  it('passes through trimmed string edits', () => {
    const patch: ReviewPatchRecord = { title: 'Old' };
    const edit: ReviewEditState = { title: '  New title  ', created: '2026-05-01' };
    expect(buildEditedReviewPatch(patch, edit)).toEqual({
      title: 'New title',
      created: '2026-05-01'
    });
  });

  it('strips the standard_metadata sidecar from the outgoing patch', () => {
    const patch: ReviewPatchRecord = {
      title: 'X',
      standard_metadata: { confidence: 0.9 }
    };
    const result = buildEditedReviewPatch(patch, {});
    expect(result).not.toBeNull();
    expect(result).not.toHaveProperty('standard_metadata');
    expect(result).toMatchObject({ title: 'X' });
  });

  it('converts numeric reference fields into integers', () => {
    const patch: ReviewPatchRecord = { correspondent: 7, document_type: 9 };
    const result = buildEditedReviewPatch(patch, {
      correspondent: '17',
      document_type: '23'
    });
    expect(result).toMatchObject({ correspondent: 17, document_type: 23 });
  });

  it('clears reference fields when the user submits an empty string', () => {
    const patch: ReviewPatchRecord = { correspondent: 7 };
    const result = buildEditedReviewPatch(patch, { correspondent: '' });
    expect(result).toMatchObject({ correspondent: null });
  });

  it('rejects non-integer or non-positive reference ids', () => {
    expect(buildEditedReviewPatch({}, { correspondent: 'abc' })).toBeNull();
    expect(buildEditedReviewPatch({}, { correspondent: '0' })).toBeNull();
    expect(buildEditedReviewPatch({}, { document_type: '-3' })).toBeNull();
    expect(buildEditedReviewPatch({}, { document_type: '1.5' })).toBeNull();
  });
});
