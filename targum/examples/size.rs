fn main() {
    println!("Quick               {}", std::mem::size_of::<targum::quick::Quick>());
    println!("Step                {}", std::mem::size_of::<targum::exec::Step>());
    println!("Trap                {}", std::mem::size_of::<targum::exec::Trap>());
    println!("Result<Step, Trap>  {}", std::mem::size_of::<Result<targum::exec::Step, targum::exec::Trap>>());
    println!("Unsupported         {}", std::mem::size_of::<targum::exec::Unsupported>());
    println!("Instruction         {}", std::mem::size_of::<iced_x86::Instruction>());
}
