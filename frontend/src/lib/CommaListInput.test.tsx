import { describe, expect, it, vi, afterEach } from 'vitest';
import { render, cleanup, fireEvent } from '@testing-library/react';
import { CommaListInput } from './ui';

afterEach(cleanup);

describe('CommaListInput', () => {
  it('keeps separators typeable and only commits on blur', () => {
    const onCommit = vi.fn();
    const { getByRole } = render(<CommaListInput values={[]} onCommit={onCommit} />);
    const input = getByRole('textbox') as HTMLInputElement;

    // Typing a comma-separated value must keep the raw text (the old
    // join/split-on-change made the comma vanish) and not commit mid-typing.
    fireEvent.change(input, { target: { value: 'alpha, beta,' } });
    expect(input.value).toBe('alpha, beta,');
    expect(onCommit).not.toHaveBeenCalled();

    // Blur parses, trims, and drops empties.
    fireEvent.blur(input);
    expect(onCommit).toHaveBeenCalledWith(['alpha', 'beta']);
  });

  it('resyncs when the committed values change externally', () => {
    const onCommit = vi.fn();
    const { getByRole, rerender } = render(
      <CommaListInput values={['one']} onCommit={onCommit} />
    );
    const input = getByRole('textbox') as HTMLInputElement;
    expect(input.value).toBe('one');

    rerender(<CommaListInput values={['one', 'two']} onCommit={onCommit} />);
    expect(input.value).toBe('one, two');
  });
});
