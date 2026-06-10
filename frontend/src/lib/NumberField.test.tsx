import { describe, expect, it, vi, afterEach } from 'vitest';
import { render, cleanup, fireEvent } from '@testing-library/react';
import { NumberField } from './ui';

afterEach(cleanup);

describe('NumberField', () => {
  it('lets a multi-digit value be typed and only clamps on blur', () => {
    const onCommit = vi.fn();
    const { getByRole } = render(
      <NumberField value={30} min={30} max={3650} onCommit={onCommit} />
    );
    const input = getByRole('spinbutton') as HTMLInputElement;

    // Typing "90" must not clamp the first digit to the minimum mid-typing
    // (the old per-keystroke clamp turned "90" into 300).
    fireEvent.change(input, { target: { value: '90' } });
    expect(input.value).toBe('90');
    expect(onCommit).not.toHaveBeenCalled();

    fireEvent.blur(input);
    expect(onCommit).toHaveBeenCalledWith(90);
  });

  it('clamps to min and never commits 0 on clear', () => {
    const onCommit = vi.fn();
    const { getByRole } = render(
      <NumberField value={10} min={1} max={365} onCommit={onCommit} />
    );
    const input = getByRole('spinbutton') as HTMLInputElement;

    fireEvent.change(input, { target: { value: '' } });
    fireEvent.blur(input);
    expect(onCommit).toHaveBeenCalledWith(1); // min, not 0
    expect(input.value).toBe('1');
  });

  it('clamps an over-max value down on blur', () => {
    const onCommit = vi.fn();
    const { getByRole } = render(
      <NumberField value={1} min={1} max={20} onCommit={onCommit} />
    );
    const input = getByRole('spinbutton') as HTMLInputElement;
    fireEvent.change(input, { target: { value: '999' } });
    fireEvent.blur(input);
    expect(onCommit).toHaveBeenCalledWith(20);
  });

  it('supports fractional values when integer=false', () => {
    const onCommit = vi.fn();
    const { getByRole } = render(
      <NumberField value={0.5} min={0} max={1} step={0.05} integer={false} onCommit={onCommit} />
    );
    const input = getByRole('spinbutton') as HTMLInputElement;
    fireEvent.change(input, { target: { value: '0.85' } });
    fireEvent.blur(input);
    expect(onCommit).toHaveBeenCalledWith(0.85);
  });
});
