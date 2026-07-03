;; Fixture with known complexity values, used by integration tests.

;; Cognitive: dotimes(+1) + nested dotimes(+2) + cond clause(+3) + else(+1 flat) = 7
;; Cyclomatic: base 1 + dotimes + dotimes + cond clause = 4
;; (Lisp's `if` is a single-decision expression, so the 7th cognitive point
;;  comes from a flat `cond` else rather than a labelled continue.)
(defun sum-of-primes (max)
  (let ((total 0))
    (dotimes (i max)
      (dotimes (j i)
        (cond ((zerop (mod i j)) (setf total total))
              (t (incf total i)))))
    total))

;; Cognitive: case(+1) = 1 ; Cyclomatic: base 1 + 2 non-default clauses = 3
(defun get-words (n)
  (case n
    (1 "one")
    (2 "a couple")
    (otherwise "lots")))

;; Cognitive: if(+1) + and(+1) = 2 ; Cyclomatic: base 1 + if + and = 3
(defun classify (a b)
  (if (and a b) "both" "not"))
