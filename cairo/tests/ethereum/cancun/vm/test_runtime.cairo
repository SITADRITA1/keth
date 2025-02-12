%builtins range_check

from starkware.cairo.common.alloc import alloc
from starkware.cairo.common.dict_access import DictAccess
from ethereum_types.bytes import Bytes, BytesStruct
from ethereum.cancun.vm.runtime import (
    get_valid_jump_destinations,
    finalize_jumpdests,
    assert_valid_jumpdest,
)
from legacy.utils.dict import dict_squash

func test__get_valid_jump_destinations{range_check_ptr}(output_ptr: felt*) {
    alloc_locals;

    tempvar bytecode_len;
    let (bytecode) = alloc();

    %{
        ids.bytecode_len = len(program_input["bytecode"])
        segments.write_arg(ids.bytecode, program_input["bytecode"])
    %}

    tempvar code = Bytes(new BytesStruct(data=bytecode, len=bytecode_len));

    let valid_jumpdests = get_valid_jump_destinations(code);
    let valid_jumpdests_ptr = valid_jumpdests.value.dict_ptr;

    %{ segments.write_arg(ids.output_ptr, __dict_manager.get_dict(ids.valid_jumpdests_ptr)) %}

    return ();
}

func test__finalize_jumpdests{range_check_ptr}() {
    alloc_locals;

    local bytecode: felt*;
    local valid_jumpdests_start: DictAccess*;
    local valid_jumpdests: DictAccess*;
    %{
        from starkware.cairo.common.dict import DictTracker
        from tests.utils.helpers import flatten
        from ethereum.cancun.vm.runtime import get_valid_jump_destinations

        memory[fp] = segments.add()
        segments.write_arg(memory[fp], program_input["bytecode"])

        data = {k: 1 for k in get_valid_jump_destinations(program_input["bytecode"])}

        base = segments.add()
        segments.load_data(
            base,
            flatten([[int(k), 1, 1] for k in data.keys()])
        )
        __dict_manager.trackers[base.segment_index] = DictTracker(
            data=data,
            current_ptr=(base + len(data) * 3),
        )
        memory[fp + 1] = base
        memory[fp + 2] = base + len(data) * 3
    %}

    let (sorted_keys_start, sorted_keys_end) = dict_squash(valid_jumpdests_start, valid_jumpdests);

    finalize_jumpdests(0, sorted_keys_start, sorted_keys_end, bytecode);

    return ();
}

func test__assert_valid_jumpdest{range_check_ptr}() {
    alloc_locals;
    tempvar bytecode: felt*;
    tempvar valid_jumpdest: DictAccess*;
    %{
        ids.bytecode = segments.add()
        segments.write_arg(ids.bytecode, program_input["bytecode"])
        ids.valid_jumpdest = segments.add()
        segments.write_arg(ids.valid_jumpdest.address_, program_input["valid_jumpdest"])
    %}
    assert_valid_jumpdest(0, bytecode, valid_jumpdest);
    return ();
}
