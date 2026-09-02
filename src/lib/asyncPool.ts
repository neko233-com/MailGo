export async function mapWithConcurrency<T, R>(
  items: readonly T[],
  concurrency: number,
  task: (item: T, index: number) => Promise<R>,
): Promise<R[]> {
  if (!Number.isSafeInteger(concurrency) || concurrency < 1) {
    throw new RangeError('concurrency must be a positive safe integer')
  }
  if (items.length === 0) return []

  const results = new Array<R>(items.length)
  let cursor = 0
  let firstError: unknown
  let failed = false

  const worker = async () => {
    while (cursor < items.length) {
      const index = cursor
      cursor += 1
      try {
        results[index] = await task(items[index], index)
      } catch (error) {
        if (!failed) firstError = error
        failed = true
      }
    }
  }

  await Promise.all(Array.from(
    { length: Math.min(concurrency, items.length) },
    () => worker(),
  ))
  if (failed) throw firstError
  return results
}
