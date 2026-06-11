import { describe, expect, it, vi, afterEach } from 'vitest';
import { render, cleanup, fireEvent } from '@testing-library/react';
import { I18nProvider } from '../../i18n/I18nProvider';
import { TuningNumberField } from './tuning';

afterEach(cleanup);

function renderField(props: Partial<Parameters<typeof TuningNumberField>[0]> = {}) {
  const onChange = vi.fn();
  const utils = render(
    <I18nProvider>
      <TuningNumberField
        field="hourly_document_limit"
        value={null}
        defaultValue={null}
        min={0}
        step={1}
        onChange={onChange}
        {...props}
      />
    </I18nProvider>
  );
  const input = utils.getByRole('spinbutton') as HTMLInputElement;
  return { ...utils, input, onChange };
}

describe('TuningNumberField', () => {
  it('commits only on blur, rounded for integer fields (#314)', () => {
    const { input, onChange } = renderField();

    // Drafting must not commit per keystroke (the old behavior pushed every
    // intermediate value, including fractions, into the settings payload).
    fireEvent.change(input, { target: { value: '2.7' } });
    expect(onChange).not.toHaveBeenCalled();

    fireEvent.blur(input);
    expect(onChange).toHaveBeenCalledWith(3); // rounded, no fraction for Option<u32>
    expect(input.value).toBe('3');
  });

  it('clamps negatives up to min instead of committing them', () => {
    const { input, onChange } = renderField({ min: 0 });
    fireEvent.change(input, { target: { value: '-5' } });
    fireEvent.blur(input);
    expect(onChange).toHaveBeenCalledWith(0);
    expect(input.value).toBe('0');
  });

  it('commits null (inherit) when cleared', () => {
    const { input, onChange } = renderField({ value: 25 });
    fireEvent.change(input, { target: { value: '' } });
    fireEvent.blur(input);
    expect(onChange).toHaveBeenCalledWith(null);
    expect(input.value).toBe('');
  });

  it('keeps fractions and clamps to max for threshold fields (integer=false)', () => {
    const { input, onChange } = renderField({
      field: 'metadata_confidence_threshold',
      min: 0,
      max: 1,
      step: 0.05,
      integer: false
    });
    fireEvent.change(input, { target: { value: '0.85' } });
    fireEvent.blur(input);
    expect(onChange).toHaveBeenCalledWith(0.85);

    fireEvent.change(input, { target: { value: '1.7' } });
    fireEvent.blur(input);
    expect(onChange).toHaveBeenLastCalledWith(1);
  });

  it('preserves an explicit zero as 0, not null', () => {
    const { input, onChange } = renderField({ min: 0 });
    fireEvent.change(input, { target: { value: '0' } });
    fireEvent.blur(input);
    expect(onChange).toHaveBeenCalledWith(0);
    expect(input.value).toBe('0');
  });
});
