; Fixture with known complexity values, used by integration tests.

; Cognitive: do(+1) + nested do(+2) + cond clause(+3) + else(+1 flat) = 7
; Cyclomatic: base 1 + do + do + cond clause = 4
; (Scheme's `if` is a single-decision expression, so the 7th cognitive point
;  comes from a flat `cond` else rather than a labelled continue.)
(define (sum-of-primes max)
  (define total 0)
  (do ((i 2 (+ i 1)))
      ((> i max) total)
    (do ((j 2 (+ j 1)))
        ((>= j i))
      (cond ((= (modulo i j) 0) (set! total total))
            (else (set! total (+ total i)))))))

; Cognitive: case(+1) = 1 ; Cyclomatic: base 1 + 2 non-default clauses = 3
(define (get-words n)
  (case n
    ((1) "one")
    ((2) "a couple")
    (else "lots")))

; Cognitive: if(+1) + and(+1) = 2 ; Cyclomatic: base 1 + if + and = 3
(define (classify a b)
  (if (and a b) "both" "not"))
