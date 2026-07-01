// Fixture with known complexity values, used by integration tests.

// Cognitive: for(+1) + nested for(+2) + nested if(+3) + labelled continue(+1) = 7
// Cyclomatic: base 1 + for + for + if = 4
fn sum_of_primes(max: u32) -> u32 {
    let mut total = 0;
    'out: for i in 2..=max {
        for j in 2..i {
            if i % j == 0 {
                continue 'out;
            }
        }
        total += i;
    }
    total
}

// Cognitive: match(+1) = 1 ; Cyclomatic: base 1 + 2 non-default arms = 3
fn get_words(n: u32) -> &'static str {
    match n {
        1 => "one",
        2 => "a couple",
        _ => "lots",
    }
}

// Cognitive: if(+1) + &&(+1) = 2 ; Cyclomatic: base 1 + if + && = 3
fn classify(a: bool, b: bool) -> &'static str {
    if a && b {
        return "both";
    }
    "not"
}
