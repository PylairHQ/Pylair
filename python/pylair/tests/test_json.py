"""Wire-codec unit tests — the JSON safety guards in pylair._json."""

import dataclasses
import enum

import pytest

from pylair import BigInt, _json

_SAFE = (1 << 53) - 1  # JS Number.MAX_SAFE_INTEGER


def test_dumps_safe_int_boundary():
    assert _json.loads(_json.dumps({"n": _SAFE}))["n"] == _SAFE
    assert _json.loads(_json.dumps({"n": -_SAFE}))["n"] == -_SAFE


def test_dumps_rejects_int_above_safe_range():
    with pytest.raises(ValueError, match="safe-integer range"):
        _json.dumps({"n": _SAFE + 1})


def test_dumps_rejects_negative_int_below_safe_range():
    with pytest.raises(ValueError, match="safe-integer range"):
        _json.dumps({"n": -(_SAFE + 1)})


def test_dumps_rejects_nested_big_int():
    with pytest.raises(ValueError, match="safe-integer range"):
        _json.dumps({"a": [{"b": 1 << 60}]})


@dataclasses.dataclass
class _Point:
    x: int
    y: int


def test_dumps_rejects_big_int_inside_dataclass():
    # The pre-walk descends into dataclass fields, so conversions in
    # _default can't smuggle an unsafe int past it.
    with pytest.raises(ValueError, match="safe-integer range"):
        _json.dumps(_Point(1, 1 << 60))


def test_dumps_rejection_message_names_workaround():
    with pytest.raises(ValueError, match="as a string"):
        _json.dumps({"n": 1 << 60})
    with pytest.raises(ValueError, match="pylair\\.BigInt"):
        _json.dumps({"n": 1 << 60})


def test_dumps_still_rejects_nan():
    with pytest.raises(ValueError):
        _json.dumps({"x": float("nan")})


def test_dumps_float_unaffected():
    # Big floats keep double semantics — only int precision is guarded.
    assert _json.loads(_json.dumps({"x": 1e300}))["x"] == 1e300


def test_check_wire_shared_container_rewalked_at_a_deeper_reference():
    # A shared container first seen shallow must be measured again when
    # reached deeper: json.dumps expands every reference, so the deeper
    # copy's contents sit deeper in the emitted text than the first walk
    # measured. Dict order walks "x" (depth 2) before "y" (depth 4); the
    # second copy puts 1 at emitted depth 6 > 5.
    shared = [[1]]
    with pytest.raises(_json.DepthExceeded):
        _json.dumps({"y": [[shared]], "x": shared}, depth_limit=5)


def test_check_wire_diamond_within_the_limit_passes():
    # Same object twice is legal JSON — both copies emit, so the walk
    # checks each occurrence's depth but validates the contents once.
    shared = [1]
    obj = {"x": shared, "y": [shared]}
    assert _json.loads(_json.dumps(obj, depth_limit=5)) == {
        "x": [1],
        "y": [[1]],
    }


def test_check_wire_cycle_terminates():
    cyc = []
    cyc.append(cyc)
    # No depth limit: revisits are skipped, the walk ends, and json.dumps
    # raises its own circular-reference error as before.
    with pytest.raises(ValueError, match="Circular"):
        _json.dumps(cyc)
    # With a limit the re-walk climbs to DepthExceeded instead —
    # either way the walk cannot spin forever.
    with pytest.raises(_json.DepthExceeded):
        _json.dumps(cyc, depth_limit=32)


# ---- input validation (coerce/coerce_kwargs) --------------------------------
# Wrong primitives must be INVALID_PARAMS at the boundary, never reach the
# function body (and never surface as an INTERNAL TypeError from the call).


def test_int_rejects_string_bool_and_fractional():
    assert _json.coerce(int, 5) == 5
    assert _json.coerce(int, 3.0) == 3  # JSON cannot tell 3 from 3.0
    for bad in ("x", 3.5, True, False, None, {"a": 1}, [1]):
        with pytest.raises(_json.InvalidParams, match="expected int"):
            _json.coerce(int, bad)


def test_float_accepts_int_rejects_bool_and_str():
    assert _json.coerce(float, 2) == 2.0
    assert _json.coerce(float, 2.5) == 2.5
    for bad in ("x", True, None, {"a": 1}):
        with pytest.raises(_json.InvalidParams, match="expected float"):
            _json.coerce(float, bad)


def test_str_and_bool_are_strict():
    assert _json.coerce(str, "x") == "x"
    assert _json.coerce(bool, True) is True
    for bad in (5, 1.5, True, None, [1]):
        with pytest.raises(_json.InvalidParams, match="expected str"):
            _json.coerce(str, bad)
    for bad in (1, 0, "true", None):
        with pytest.raises(_json.InvalidParams, match="expected bool"):
            _json.coerce(bool, bad)


def test_optional_and_union_accept_matching_arm():
    opt = int | None
    assert _json.coerce(opt, None) is None
    assert _json.coerce(opt, 4) == 4
    with pytest.raises(_json.InvalidParams):
        _json.coerce(opt, "x")
    union = int | str
    assert _json.coerce(union, 4) == 4
    assert _json.coerce(union, "x") == "x"
    with pytest.raises(_json.InvalidParams):
        _json.coerce(union, 3.5)


def test_structured_failures_raise_not_passthrough():
    @dataclasses.dataclass
    class P:
        x: int

    class E(enum.Enum):
        A = "a"

    for ann, bad in [
        (P, 5),
        (list[int], "x"),
        (list[int], [1, "x"]),  # element validation recurses
        (dict[str, int], {"k": "v"}),
        (E, "nope"),
        (bytes, 5),
    ]:
        with pytest.raises(_json.InvalidParams):
            _json.coerce(ann, bad)


def test_kwargs_missing_and_extra_keys():
    def f(a: int, b: int = 0):
        return a + b

    assert _json.coerce_kwargs(f, {"a": 1}) == {"a": 1}
    with pytest.raises(_json.InvalidParams, match="missing.*'a'"):
        _json.coerce_kwargs(f, {"b": 1})
    with pytest.raises(_json.InvalidParams, match="unexpected.*'c'"):
        _json.coerce_kwargs(f, {"a": 1, "c": 2})


def test_kwargs_unannotated_param_passes_through():
    def f(x, y: int = 0):
        return x

    out = _json.coerce_kwargs(f, {"x": "anything", "y": 3})
    assert out == {"x": "anything", "y": 3}


def test_kwargs_message_truncates_large_values():
    def f(a: int):
        return a

    try:
        _json.coerce_kwargs(f, {"a": "x" * 5000})
    except _json.InvalidParams as e:
        assert "param 'a'" in str(e)
        assert "expected int" in str(e)
        assert "x" * 5000 not in str(e)
    else:
        raise AssertionError("expected InvalidParams")


def test_kwargs_var_keyword_allows_extras():
    def f(a: int, **kw):
        return a

    assert _json.coerce_kwargs(f, {"a": 1, "z": 9})["z"] == 9


def test_kwargs_none_default_admits_none_without_optional():
    # `x: str = None` — pre-Optional style; the None default makes
    # null a legal value even though the bare annotation rejects it.
    def f(x: str = None):  # noqa: RUF013
        return x

    assert _json.coerce_kwargs(f, {"x": None}) == {"x": None}
    assert _json.coerce_kwargs(f, {"x": "s"}) == {"x": "s"}
    assert _json.coerce_kwargs(f, {}) == {}
    # …but a non-None value is still validated against the annotation.
    with pytest.raises(_json.InvalidParams, match="param 'x'.*expected str"):
        _json.coerce_kwargs(f, {"x": 5})

    # A non-None default does NOT admit null.
    def g(x: str = "d"):
        return x

    with pytest.raises(_json.InvalidParams, match="expected str, got null"):
        _json.coerce_kwargs(g, {"x": None})


# ---- pylair.BigInt tagging --------------------------------------------------
# Opt-in by type: a BigInt ALWAYS encodes {"$bigint": "<decimal>"} — even a
# small one — and plain int is untouched (safe range is numbers, outside is
# the existing clear error, now naming the way out).


def test_bigint_encodes_as_tag_regardless_of_size():
    assert _json.dumps(BigInt(5)) == '{"$bigint":"5"}'
    assert _json.dumps(BigInt(-42)) == '{"$bigint":"-42"}'
    assert _json.dumps(BigInt(0)) == '{"$bigint":"0"}'
    big = 1 << 70  # beyond ±2^63 too — the tag is decimal text
    assert _json.dumps(BigInt(big)) == f'{{"$bigint":"{big}"}}'


def test_bigint_encodes_nested_and_in_containers():
    out = _json.dumps({"l": [BigInt(3), 2], "d": {"k": BigInt(-1)}, "t": (BigInt(9),)})
    assert '"l":[{"$bigint":"3"},2]' in out
    assert '"d":{"k":{"$bigint":"-1"}}' in out
    assert '"t":[{"$bigint":"9"}]' in out


def test_bigint_inside_dataclass_field_is_tagged():
    @dataclasses.dataclass
    class R:
        n: BigInt

    assert _json.dumps(R(BigInt(1 << 62))) == f'{{"n":{{"$bigint":"{1 << 62}"}}}}'


def test_bigint_in_set_emits_sorted_tag_list():
    assert _json.dumps({BigInt(2), BigInt(10)}) == '[{"$bigint":"10"},{"$bigint":"2"}]'


def test_plain_int_and_bool_never_tagged():
    assert _json.dumps(5) == "5"
    assert _json.dumps(True) == "true"
    assert _json.dumps({"b": True, "n": 7}) == '{"b":true,"n":7}'
    # An int that happens to live inside a BigInt-typed world: only the
    # typed value tags — neighbours stay plain numbers.
    assert _json.dumps([BigInt(1), 1]) == '[{"$bigint":"1"},1]'


def test_bigint_encode_over_digit_cap_rejects():
    with pytest.raises(ValueError, match="digit wire cap"):
        _json.dumps(BigInt(10**4096))
    # At the cap is still fine.
    assert "$bigint" in _json.dumps(BigInt(10**4095))


def test_loads_bigint_tag_decodes():
    v = _json.loads('{"a": {"$bigint": "12345678901234567890"}, "b": 1}')
    assert v["a"] == BigInt("12345678901234567890")
    assert isinstance(v["a"], BigInt)
    assert v["b"] == 1
    assert _json.loads('{"$bigint": "-7"}') == BigInt(-7)
    assert _json.loads('{"$bigint": "0"}') == BigInt(0)


def test_loads_bigint_malformed_and_extra_keys_pass_through():
    # Anything but {exactly "$bigint": canonical-decimal} is plain data —
    # same defensive rule as $b64/$blob.
    for raw in [
        '{"$bigint": "01"}',  # leading zero — not canonical
        '{"$bigint": "+5"}',
        '{"$bigint": "5.0"}',
        '{"$bigint": "abc"}',
        '{"$bigint": ""}',
        '{"$bigint": 5}',  # non-string payload
        '{"$bigint": "5", "x": 1}',  # extra key
        '{"$bigint": "5 ", "y": 2}',  # whitespace + extra key
    ]:
        out = _json.loads(raw)
        assert isinstance(out, dict), raw
        assert not isinstance(out["$bigint"], BigInt), raw


def test_loads_bigint_over_digit_cap_rejects():
    with pytest.raises(ValueError, match="digit cap"):
        _json.loads('{"$bigint": "' + "9" * 4097 + '"}')
    # Exactly at the cap is still a valid decode.
    assert _json.loads('{"$bigint": "' + "9" * 4096 + '"}') == BigInt(10**4096 - 1)


def test_bigint_round_trip():
    payload = {"n": BigInt(-(1 << 80)), "list": [BigInt(3), {"deep": BigInt(0)}]}
    back = _json.loads(_json.dumps(payload))
    assert back == payload
    assert isinstance(back["n"], BigInt)
    assert isinstance(back["list"][1]["deep"], BigInt)


def test_coerce_bigint_param():
    assert _json.coerce(BigInt, 5) == BigInt(5)
    assert isinstance(_json.coerce(BigInt, 5), BigInt)
    assert _json.coerce(BigInt, BigInt(9)) == BigInt(9)
    for bad in (True, "5", 3.5, None, {"$bigint": "5"}, [1]):
        with pytest.raises(_json.InvalidParams, match="expected BigInt"):
            _json.coerce(BigInt, bad)
