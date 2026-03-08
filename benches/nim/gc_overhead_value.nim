import std/os
import std/strutils

type
  Boxed = object
    v: int
  Pair = object
    x: int
    y: int

proc argInt(index: int; defaultValue: int): int =
  if paramCount() >= index:
    try:
      parseInt(paramStr(index))
    except ValueError:
      defaultValue
  else:
    defaultValue

proc ctorConsume(v: Boxed): int =
  let one = 1
  let two = one + 1
  let three = two + 1
  let four = three + 1
  let five = four + 1
  v.v + five

proc ctorRunBatch(batch: int; n: int; acc: int): int =
  if batch == 0:
    acc
  else:
    let x = Boxed(v: n)
    let iter = ctorConsume(x)
    ctorRunBatch(batch - 1, n - 1, acc + iter)

proc aliasConsume(v: Pair; bias: int): int =
  let one = 1
  let two = one + 1
  v.x + v.y + two + bias

proc aliasChurnOnce(n: int; acc: int): int =
  if n == 0:
    acc
  else:
    let p = Pair(x: n, y: acc)
    let a = p
    let b = a
    let next = aliasConsume(b, 0)
    aliasChurnOnce(n - 1, next)

proc aliasRunBatch(batch: int; n: int; acc: int): int =
  if batch == 0:
    acc
  else:
    let iter = aliasChurnOnce(n, 0)
    aliasRunBatch(batch - 1, n, acc + iter)

proc branchConsume(v: Boxed; dir: bool; acc: int): int =
  let one = 1
  let two = one + 1
  let three = two + 1
  if dir:
    acc + v.v + three
  else:
    acc - v.v - three

proc branchChurnOnce(n: int; acc: int): int =
  if n == 0:
    acc
  else:
    let x = Boxed(v: n)
    let dir = n == 1
    let next = branchConsume(x, dir, acc)
    branchChurnOnce(n - 1, next)

proc branchRunBatch(batch: int; n: int; acc: int): int =
  if batch == 0:
    acc
  else:
    let iter = branchChurnOnce(n, 0)
    branchRunBatch(batch - 1, n, acc + iter)

when defined(ctor_churn):
  const defaultN = 4000
  const defaultBatch = 300
  proc runCase(batch: int; n: int): int =
    ctorRunBatch(batch, n, 0)
elif defined(alias_churn):
  const defaultN = 2500
  const defaultBatch = 120
  proc runCase(batch: int; n: int): int =
    aliasRunBatch(batch, n, 0)
elif defined(branch_churn):
  const defaultN = 3000
  const defaultBatch = 90
  proc runCase(batch: int; n: int): int =
    branchRunBatch(batch, n, 0)
else:
  {.fatal: "define one case: -d:ctor_churn | -d:alias_churn | -d:branch_churn".}

when isMainModule:
  let n = argInt(1, defaultN)
  let batch = argInt(2, defaultBatch)
  let runs = argInt(3, 1)
  var sink = 0
  for _ in 0..<runs:
    sink = runCase(batch, n)
  if sink == int.low:
    echo sink
  quit(0)
