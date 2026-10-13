
            (define (sum-evens lst)
              (for/sum ((n (in-list lst)) #:when (even? n))
                n))
        